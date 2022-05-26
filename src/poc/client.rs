use crate::{
    error::ServiceError,
    gateway,
    poc::{Onion, PocId, PocStore, QueueChallenge, QueueReport},
    service::gateway::{Challenge, ChallengeCheck, GatewayService},
    KeyedUri, Keypair, Packet, Result, Settings, ToValue,
};
use futures::{
    stream::{self, StreamExt},
    TryFutureExt,
};
use helium_proto::BlockchainRegionParamsV1;
use slog::{error, info, o, warn, Logger};
use std::sync::Arc;
use tokio::{
    sync::mpsc,
    time::{self, Duration, MissedTickBehavior},
};

pub const CHECK_QUEUE_INTERVAL: Duration = Duration::from_secs(30);
pub const MAX_REPORT_RETRY_COUNT: i8 = 20;

pub const POC_CONFIG_KEYS: &[&str] = &["block_time", "poc_timeout"];
pub const POC_ANCIENT_DURATION: Duration = Duration::from_secs(60_000);

#[derive(Debug)]
pub enum Message {
    PocChallenge(Challenge),
    PocPacket(Packet),
    GatewayChanged(Option<GatewayService>),
    ConfigChanged(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct MessageSender(pub(crate) mpsc::Sender<Message>);
pub type MessageReceiver = mpsc::Receiver<Message>;

pub fn message_channel(size: usize) -> (MessageSender, MessageReceiver) {
    let (tx, rx) = mpsc::channel(size);
    (MessageSender(tx), rx)
}

impl MessageSender {
    pub async fn gateway_changed(&self, gateway: Option<GatewayService>) {
        let _ = self.0.send(Message::GatewayChanged(gateway)).await;
    }

    pub async fn poc_challenge(&self, challenge: Challenge) {
        let _ = self.0.send(Message::PocChallenge(challenge)).await;
    }

    pub async fn poc_packet(&self, packet: Packet) {
        let _ = self.0.send(Message::PocPacket(packet)).await;
    }

    pub async fn config_changed(&self, keys: Vec<String>) {
        let _ = self.0.send(Message::ConfigChanged(keys)).await;
    }
}

pub struct PocClient {
    keypair: Arc<Keypair>,
    gateway: Option<GatewayService>,
    messages: MessageReceiver,
    downlinks: gateway::MessageSender,
    block_time: Option<Duration>,
    poc_timeout: Option<u8>,
    region_params: Option<BlockchainRegionParamsV1>,
    store: PocStore,
}

impl PocClient {
    pub fn new(
        messages: MessageReceiver,
        downlinks: gateway::MessageSender,
        settings: &Settings,
    ) -> Result<Self> {
        let store = PocStore::new(&settings.cache);
        Ok(Self {
            keypair: settings.keypair.clone(),
            gateway: None,
            messages,
            downlinks,
            store,
            region_params: None,
            poc_timeout: None,
            block_time: None,
        })
    }

    pub async fn run(&mut self, shutdown: triggered::Listener, logger: &Logger) -> Result {
        let logger = logger.new(o!(
            "module" => "poc",
        ));
        info!(logger, "starting");

        let mut queue_timer = time::interval(CHECK_QUEUE_INTERVAL);
        queue_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown.clone() => {
                    info!(logger, "shutting down");
                    return Ok(())
                },
                message = self.messages.recv() => match message {
                    Some(Message::GatewayChanged(gateway)) => {
                        info!(logger, "gateway changed");
                        self.gateway = gateway;
                        self.block_time = None;
                        self.poc_timeout = None;
                    }
                    Some(Message::PocChallenge(challenge)) => {
                        let _ = self.handle_challenge(&logger, challenge)
                            .inspect_err(|err| warn!(logger, "ignoring failed challenge {:?}", err))
                            .await;
                    },
                    Some(Message::PocPacket(packet)) => {
                        let _ = self.handle_packet(&logger, &packet)
                            .inspect_err(|err| warn!(logger, "ignoring failed packet {:?}", err))
                            .await;
                    },
                    Some(Message::ConfigChanged(keys)) => {
                        if keys.iter().any(|needle| POC_CONFIG_KEYS.contains(&needle.as_str())) {
                            info!(logger, "received config change");
                            self.block_time = None;
                            self.poc_timeout = None;
                        }
                    }
                    None => warn!(logger, "ignoring closed message channel"),
                },
                _ = queue_timer.tick() => {
                    self.handle_queue_timer_tick(&logger).await;
                }
            }
        }
    }

    // A poc packet received over the air that decodes as an onion is handled as
    // a witness report and queued for delivery
    async fn handle_packet(&mut self, logger: &Logger, packet: &Packet) -> Result {
        let onion = Onion::from_packet(packet, self.region_params.as_ref())?;
        let report = onion.as_witness_report(self.keypair.clone()).await?;
        self.store.store_waiting_report(onion.poc_id(), report)?;
        info!(logger, "queued witness report";
            "poc_id" => onion.poc_id(),
            "rssi" => onion.signal_strength,
            "freq" => onion.frequency,
            "snr" => onion.snr
        );
        Ok(())
    }

    // A challenge received from the connected gateway is checked against the
    // challenger
    async fn handle_challenge(&mut self, logger: &Logger, challenge: Challenge) -> Result {
        let poc_id = challenge.poc_id.clone();
        let challenger = challenge.challenger.clone();
        self.store
            .store_waiting_challenge(challenge.poc_id.clone(), challenge)?;
        info!(logger, "queued challenge";
            "poc_id" => poc_id,
            "challenger_key" => challenger.pubkey.to_string(),
            "challenger_uri" => challenger.uri.to_string(),
        );
        Ok(())
    }

    async fn _get_config(&mut self) {
        if let Some(gateway) = self.gateway.as_mut() {
            if let Ok([block_time, poc_timeout]) = gateway.config(POC_CONFIG_KEYS).await.as_deref()
            {
                self.block_time = block_time.to_value().ok().map(Duration::from_millis);
                self.poc_timeout = poc_timeout.to_value().ok().map(|v| v as u8);
            }
        }
    }

    async fn poc_timeout(&mut self) -> Option<u8> {
        if self.poc_timeout.is_none() {
            self._get_config();
        }
        self.poc_timeout
    }

    async fn block_time(&mut self) -> Option<Duration> {
        if self.block_time.is_none() {
            self._get_config();
        };
        self.block_time
    }

    async fn handle_queue_timer_tick(&mut self, logger: &Logger) {
        // Process pending reports
        stream::iter(self.store.waiting_reports_mut())
            .for_each_concurrent(5, |(poc_id, report)| async {
                let report_type = report.report_type();
                match process_queued_report(&mut self.gateway.clone(), poc_id, report).await {
                    Ok(()) => {
                        // Completed, mark as done
                        info!(logger, "delivered {report_type} report";
                            "poc_id" => poc_id.to_string());
                        report.retry_count = -1
                    }
                    Err(err) => {
                        // Error, increase retry count, log if done retrying
                        report.retry_count += 1;
                        if report.retry_count > MAX_REPORT_RETRY_COUNT {
                            warn!(logger, "dropping {report_type} report, max retries exceeded"; 
                                "poc_id" => poc_id.to_string());
                        } else {
                            warn!(logger, "failed to deliver {report_type} report: {err:?}";
                                "poc_id" => poc_id.to_string(),
                                "retry" => report.retry_count);
                        }
                    }
                }
            })
            .await;
        self.store.gc_waiting_reports(MAX_REPORT_RETRY_COUNT);
    }

    async fn process_queued_challenge(
        &mut self,
        logger: &Logger,
        queue_challenge: &mut QueueChallenge,
    ) -> Result {
        let challenge = &queue_challenge.challenge;
        let mut challenger = GatewayService::new(&challenge.challenger)?;
        match challenger
            .poc_check_challenge_target(self.keypair.clone(), challenge)
            .await
        {
            // Not the target of this challenge
            Ok(ChallengeCheck::NotTarget) => {
                info!(logger, "ignoring challenge, not target";
                    "poc_id" => &challenge.poc_id);
                queue_challenge.received -= POC_ANCIENT_DURATION;
                Ok(())
            }
            Ok(ChallengeCheck::Target(onion)) => {
                let mut onion = Onion::from_bytes(&onion, self.region_params.as_ref())?;
                match onion.decrypt_in_place(self.keypair.clone()) {
                    Ok(()) => {
                        let (data, next_layer) = onion.get_layer()?;
                        
                    }
                    Err(err) => {
                        error!(logger, "failed to decrypt challenge: {err:?}"; 
                            "poc_id" => onion.poc_id())
                    }
                }
                Ok(())
            }
            // The POC key exists but the POC itself may not yet be initialised
            // this can happen if the challenging validator is behind our
            // notifying validator if the challenger is behind the notifier or
            // hasn't started processing the challenge block yet, then cache
            // the check target req it will then be retried periodically
            Ok(ChallengeCheck::Queued(_)) => Ok(()),
            // An error occured talking to the challenger, leave in retry
            Err(err) => {
                warn!(logger, "failed to communicate with challenger: {err:?}";
                    "poc_id" => &challenge.poc_id,
                    "challenger_key" => challenge.challenger.pubkey.to_string(),
                    "challenger_uri" => challenge.challenger.uri.to_string());
                Ok(())
            }
        }
    }
}

async fn process_queued_report(
    gateway: &mut Option<GatewayService>,
    poc_id: &PocId,
    report: &mut QueueReport,
) -> Result {
    if report.challenger.is_none() {
        report.challenger = find_challenger(gateway, poc_id).await.unwrap_or(None)
    };

    if let Some(uri) = &report.challenger {
        let mut challenger = GatewayService::new(uri)?;
        challenger.poc_send_report(&report.report).await
    } else {
        Err(ServiceError::no_service())
    }
}

async fn find_challenger(
    gateway: &mut Option<GatewayService>,
    poc_id: &PocId,
) -> Result<Option<KeyedUri>> {
    if let Some(gateway) = gateway.as_mut() {
        gateway.poc_challenger(poc_id.as_ref()).await
    } else {
        Ok(None)
    }
}
