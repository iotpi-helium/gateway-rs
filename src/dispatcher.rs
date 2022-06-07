use crate::{
    error::ServiceError,
    gateway, poc,
    router::{self, RouterClient, Routing},
    service::{
        self,
        gateway::{Challenge, GatewayService, Response},
    },
    sync, CacheSettings, Error, KeyedUri, Keypair, Packet, Region, Result, Settings,
};
use exponential_backoff::Backoff;
use futures::{
    task::{Context, Poll},
    TryFutureExt,
};
use helium_proto::{BlockchainVarV1, GatewayRespV1};
use slog::{debug, info, o, warn, Logger};
use slog_scope;
use std::{collections::HashMap, pin::Pin, sync::Arc, time::Duration};
use tokio::{task::JoinHandle, time};
use tokio_stream::{self, StreamExt, StreamMap};

#[derive(Debug)]
pub enum Message {
    Uplink(Packet),
    PocPacket(Packet),
    Config {
        keys: Vec<String>,
        response: sync::ResponseSender<Result<Vec<BlockchainVarV1>>>,
    },
    Height {
        response: sync::ResponseSender<Result<HeightResponse>>,
    },
    Region {
        response: sync::ResponseSender<Result<Region>>,
    },
}

#[derive(Debug)]
pub struct HeightResponse {
    pub gateway: KeyedUri,
    pub height: u64,
    pub block_age: u64,
    pub gateway_version: Option<u64>,
}

pub type MessageSender = sync::MessageSender<Message>;
pub type MessageReceiver = sync::MessageReceiver<Message>;

pub fn message_channel(size: usize) -> (MessageSender, MessageReceiver) {
    sync::message_channel(size)
}

impl MessageSender {
    pub async fn config(&self, keys: &[String]) -> Result<Vec<BlockchainVarV1>> {
        let (tx, rx) = sync::response_channel();
        let _ = self
            .0
            .send(Message::Config {
                keys: keys.to_vec(),
                response: tx,
            })
            .await;
        rx.recv().await?
    }

    pub async fn uplink(&self, packet: Packet) -> Result {
        self.0
            .send(Message::Uplink(packet))
            .map_err(|_| Error::channel())
            .await
    }

    pub async fn poc_packet(&self, packet: Packet) -> Result {
        self.0
            .send(Message::PocPacket(packet))
            .map_err(|_| Error::channel())
            .await
    }

    pub async fn height(&self) -> Result<HeightResponse> {
        let (tx, rx) = sync::response_channel();
        let _ = self.0.send(Message::Height { response: tx }).await;
        rx.recv().await?
    }

    pub async fn region(&self) -> Result<Region> {
        let (tx, rx) = sync::response_channel();
        let _ = self.0.send(Message::Region { response: tx }).await;
        rx.recv().await?
    }
}

pub struct Dispatcher {
    keypair: Arc<Keypair>,
    region: Region,
    messages: MessageReceiver,
    downlinks: gateway::MessageSender,
    seed_gateways: Vec<KeyedUri>,
    routing_height: u64,
    region_height: u64,
    cache_settings: CacheSettings,
    gateway_retry: u32,
    routers: HashMap<RouterKey, RouterEntry>,
    default_routers: Option<Vec<KeyedUri>>,
    poc_client: poc::client::MessageSender,
}

#[derive(PartialEq, Eq, Hash)]
struct RouterKey {
    oui: u32,
    uri: KeyedUri,
}

#[derive(Debug)]
struct RouterEntry {
    routing: Routing,
    dispatch: router::client::MessageSender,
    join_handle: JoinHandle<Result>,
}

const GATEWAY_BACKOFF_RETRIES: u32 = 10;
const GATEWAY_BACKOFF_MIN_WAIT: Duration = Duration::from_secs(5);
const GATEWAY_BACKOFF_MAX_WAIT: Duration = Duration::from_secs(1800); // 30 minutes

const GATEWAY_MAX_BLOCK_AGE_SECS: u64 = 600;
const GATEWAY_MAX_BLOCK_AGE: Duration = Duration::from_secs(GATEWAY_MAX_BLOCK_AGE_SECS);
const GATEWAY_CHECK_INTERVAL: Duration = Duration::from_secs(GATEWAY_MAX_BLOCK_AGE_SECS / 2);

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
enum GatewayStream {
    Routing,
    Region,
    Config,
    Poc,
}

impl GatewayStream {
    async fn handle_message(
        &self,
        message: &GatewayRespV1,
        dispatcher: &mut Dispatcher,
        gateway: &mut GatewayService,
        shutdown: &triggered::Listener,
        logger: &Logger,
    ) {
        match self {
            Self::Routing => {
                dispatcher
                    .handle_routing_update(gateway, message, shutdown, logger)
                    .await
            }
            Self::Region => dispatcher.handle_region_update(message, logger).await,
            Self::Poc => dispatcher.handle_poc_challenge(message, logger).await,
            Self::Config => dispatcher.handle_config_change(message, logger).await,
        }
    }

    fn handle_error(&self, err: &Error, logger: &Logger) -> Result {
        let stream = match self {
            Self::Routing => "routing",
            Self::Region => "region",
            Self::Poc => "poc",
            Self::Config => "config",
        };
        warn!(logger, "gateway {stream} stream error: {err:?}");
        Ok(())
    }

    async fn get_stream(
        &self,
        dispatcher: &Dispatcher,
        mut gateway: GatewayService,
    ) -> Result<service::gateway::Streaming> {
        match self {
            Self::Routing => gateway.routing_stream(dispatcher.routing_height).await,
            Self::Region => {
                gateway
                    .region_params_stream(dispatcher.keypair.clone())
                    .await
            }
            Self::Poc => gateway.poc_stream(dispatcher.keypair.clone()).await,
            Self::Config => gateway.config_stream().await,
        }
    }
}

type GatewayStreams = StreamMap<GatewayStream, service::gateway::Streaming>;

impl Dispatcher {
    // Allow mutable key type for HashMap with Uri in the key
    #[allow(clippy::mutable_key_type)]
    pub fn new(
        messages: MessageReceiver,
        downlinks: gateway::MessageSender,
        poc_client: poc::client::MessageSender,
        settings: &Settings,
    ) -> Result<Self> {
        let seed_gateways = settings.gateways.clone();
        let routers = HashMap::with_capacity(5);
        let default_routers = settings.routers.clone();
        let cache_settings = settings.cache.clone();
        Ok(Self {
            keypair: settings.keypair.clone(),
            region: settings.region,
            messages,
            downlinks,
            seed_gateways,
            routers,
            routing_height: 0,
            region_height: 0,
            default_routers,
            cache_settings,
            gateway_retry: 0,
            poc_client,
        })
    }

    pub async fn run(&mut self, shutdown: triggered::Listener, logger: &Logger) -> Result {
        let logger = logger.new(o!("module" => "dispatcher"));
        info!(logger, "starting"; 
            "region" => self.region.to_string());

        if let Some(default_routers) = &self.default_routers {
            for default_router in default_routers {
                info!(logger, "default router";
                    "pubkey" => default_router.pubkey.to_string(),
                    "uri" => default_router.uri.to_string());
            }
        }

        let gateway_backoff = Backoff::new(
            GATEWAY_BACKOFF_RETRIES,
            GATEWAY_BACKOFF_MIN_WAIT,
            GATEWAY_BACKOFF_MAX_WAIT,
        );
        loop {
            if shutdown.is_triggered() {
                // Prevent unneeded seed reselection
                return Ok(());
            }
            // Select seed
            let seed_gateway = GatewayService::select_seed(&self.seed_gateways)?;
            info!(logger, "seed gateway";
                "pubkey" => seed_gateway.uri.pubkey.to_string(),
                "uri" => seed_gateway.uri.uri.to_string());

            tokio::select! {
                    _ = shutdown.clone() => {
                        info!(logger, "shutting down");
                        return Ok(())
                    },
                // Try to select a random validator from the seed and fetch the needed streams
                gateway = Self::select_gateway(seed_gateway, &shutdown, &logger)
                    .and_then(|service | self.setup_gateway_streams(service, &logger))
                     => match gateway {
                        Ok(Some((service, gateway_streams))) =>
                            self.run_with_gateway(service, gateway_streams, shutdown.clone(), &logger)
                                .await?,
                        Ok(None) =>
                            return Ok(()),
                        Err(_err) => ()
                    }
            }

            self.prepare_gateway_change(&gateway_backoff, shutdown.clone(), &logger)
                .await;
        }
    }

    async fn select_gateway(
        mut seed_gateway: GatewayService,
        shutdown: &triggered::Listener,
        logger: &Logger,
    ) -> Result<Option<GatewayService>> {
        match seed_gateway.random_new(5, shutdown.clone()).await {
            Ok(result) => Ok(result),
            Err(err) => {
                warn!(logger, "gateway selection error: {err:?}";
                    "pubkey" => seed_gateway.uri.pubkey.to_string(),
                    "uri" => seed_gateway.uri.uri.to_string());
                Err(err)
            }
        }
    }

    async fn setup_gateway_streams(
        &mut self,
        gateway: Option<GatewayService>,
        logger: &Logger,
    ) -> Result<Option<(GatewayService, GatewayStreams)>> {
        if gateway.is_none() {
            return Ok(None);
        }
        let gateway = gateway.unwrap();

        let stream_names = [
            GatewayStream::Routing,
            GatewayStream::Region,
            GatewayStream::Config,
            GatewayStream::Poc,
        ];
        let streams = stream_names
            .iter()
            .map(|name| name.get_stream(self, gateway.clone()));

        match futures_util::future::try_join_all(streams).await {
            Ok(streams) => {
                let stream_map = StreamMap::from_iter(stream_names.into_iter().zip(streams));
                Ok(Some((gateway, stream_map)))
            }
            Err(err) => {
                warn!(logger, "gateway stream setup error: {err:?} "; 
                    "pubkey" => gateway.uri.pubkey.to_string(),
                    "uri" => gateway.uri.uri.to_string());
                Err(err)
            }
        }
    }

    async fn run_with_gateway(
        &mut self,
        mut gateway: GatewayService,
        mut streams: GatewayStreams,
        shutdown: triggered::Listener,
        logger: &Logger,
    ) -> Result {
        info!(logger, "using gateway";
            "pubkey" => gateway.uri.pubkey.to_string(),
            "uri" => gateway.uri.uri.to_string());

        // Notify of gateway change
        self.notify_gateway_change(Some(gateway.clone())).await;
        // Initialize liveness check for gateway
        let mut gateway_check = time::interval(GATEWAY_CHECK_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.clone() => {
                    info!(logger, "shutting down");
                    return Ok(())
                },
                gateway_message = streams.next() => match gateway_message {
                    Some((gateway_stream, Ok(gateway_message))) =>
                        gateway_stream.handle_message(&gateway_message, self, &mut gateway, &shutdown, logger).await,
                    Some((gateway_stream, Err(err))) =>  {
                        return gateway_stream.handle_error(&err, logger);
                    },
                    None => {
                        warn!(logger, "gateway streams closed");
                        return Ok(());
                }
                },
                _ = gateway_check.tick() => match self.check_gateway(&mut gateway, logger).await {
                    Ok(()) => {
                        self.gateway_retry = 0
                    },
                    Err(err) => {
                        warn!(logger, "gateway check error: {err}");
                        return Ok(())
                    }
                },
                message = self.messages.recv() => match message {
                    Some(message) => self.handle_message(message, Some(&mut gateway.clone()), logger).await,
                    None => {
                        warn!(logger, "messages channel closed");
                        return Ok(())
                    }
                }
            }
        }
    }

    async fn check_gateway(&mut self, gateway: &mut GatewayService, logger: &Logger) -> Result {
        let (_, block_age) = gateway.height().await?;
        info!(logger, "checking gateway"; 
            "pubkey" => gateway.uri.pubkey.to_string(),
            "block_age" => block_age);
        if block_age > GATEWAY_MAX_BLOCK_AGE.as_secs() {
            return Err(Error::gateway_service_check(
                block_age,
                GATEWAY_MAX_BLOCK_AGE.as_secs(),
            ));
        }
        Ok(())
    }

    async fn notify_gateway_change(&self, gateway: Option<GatewayService>) {
        for router_entry in self.routers.values() {
            router_entry.dispatch.gateway_changed(gateway.clone()).await;
        }
    }

    async fn prepare_gateway_change(
        &mut self,
        backoff: &Backoff,
        shutdown: triggered::Listener,
        logger: &Logger,
    ) {
        // Check if shutdown trigger already happened
        if shutdown.is_triggered() {
            return;
        }
        // Tell routers to clear their gateway entries
        self.notify_gateway_change(None).await;

        // Reset routing and region heigth for the next gateway
        self.routing_height = 0;
        self.region_height = 0;

        // Use backof to sleep exponentially longer
        self.gateway_retry += 1;
        let sleep = backoff
            .next(self.gateway_retry)
            .unwrap_or(GATEWAY_BACKOFF_MAX_WAIT);

        // Select over either shutdown or sleep, and handle messages that don't
        // require a gateway
        info!(logger, "selecting new gateway in {}s", sleep.as_secs());
        tokio::select! {
            _ = shutdown => {},
            _ = time::sleep(sleep) => {}
            message = self.messages.recv() => match message {
                Some(message) => self.handle_message(message, None, logger).await,
                None => warn!(logger, "ignoring closed messages channel"),
            }
        }
    }

    async fn handle_message(
        &self,
        message: Message,
        gateway: Option<&mut GatewayService>,
        logger: &Logger,
    ) {
        match message {
            Message::Uplink(packet) => self.handle_uplink(&packet, logger).await,
            Message::PocPacket(packet) => self.handle_poc_packet(packet, logger).await,
            Message::Config { keys, response } => {
                let reply = if let Some(gateway) = gateway {
                    gateway.config(&keys).await
                } else {
                    Err(ServiceError::no_service())
                };
                response.send(reply, logger)
            }
            Message::Height { response } => {
                let reply = if let Some(gateway) = gateway {
                    let gateway_version = gateway.version().await.unwrap_or(None);
                    gateway
                        .height()
                        .await
                        .map(|(height, block_age)| HeightResponse {
                            gateway: gateway.uri.clone(),
                            height,
                            block_age,
                            gateway_version,
                        })
                } else {
                    Err(ServiceError::no_service())
                };
                response.send(reply, logger)
            }
            Message::Region { response } => response.send(Ok(self.region), logger),
        }
    }

    async fn handle_uplink(&self, packet: &Packet, logger: &Logger) {
        let mut handled = false;
        for router_entry in self.routers.values() {
            if router_entry.routing.matches_routing_info(packet.routing()) {
                match router_entry.dispatch.uplink(packet.clone()).await {
                    Ok(()) => (),
                    Err(err) => warn!(logger, "ignoring router dispatch error: {err:?}"),
                }
                handled = true;
            }
        }
        if !handled {
            if let Some(default_routers) = &self.default_routers {
                for (router_key, router_entry) in &self.routers {
                    if default_routers.contains(&router_key.uri) {
                        debug!(logger, "sending to default router");
                        let _ = router_entry.dispatch.uplink(packet.clone()).await;
                    }
                }
            }
        }
    }

    async fn handle_poc_challenge(&mut self, response: &GatewayRespV1, logger: &Logger) {
        match Challenge::try_from(response) {
            Ok(challenge) => self.poc_client.poc_challenge(challenge).await,
            Err(err) => {
                warn!(logger, "error decoding poc challenge: {err:?}");
            }
        }
    }

    async fn handle_poc_packet(&self, packet: Packet, _logger: &Logger) {
        self.poc_client.poc_packet(packet).await;
    }

    async fn handle_config_change(&mut self, response: &GatewayRespV1, logger: &Logger) {
        match response.config_update() {
            Ok(update) => self.poc_client.config_changed(update.keys.clone()).await,
            Err(err) => warn!(logger, "ignoring invalid config update: {:?}", err),
        }
    }

    async fn handle_region_update<R: service::gateway::Response>(
        &mut self,
        response: &R,
        logger: &Logger,
    ) {
        let update_height = response.height();
        let current_height = self.region_height;
        if update_height <= self.region_height {
            warn!(
                logger,
                "region returned invalid height {update_height} while at {current_height}"
            );
            return;
        }
        match response.region() {
            Ok(region) => {
                self.region_height = update_height;
                self.region = region;
                info!(
                    logger,
                    "updated region to {region} at height {update_height}"
                );
                // Tell routers about it
                for router_entry in self.routers.values() {
                    router_entry.dispatch.region_changed(region).await;
                }
            }
            Err(err) => {
                warn!(logger, "error decoding region: {err:?}");
            }
        }
    }

    async fn handle_routing_update<R: service::gateway::Response>(
        &mut self,
        gateway: &mut GatewayService,
        response: &R,
        shutdown: &triggered::Listener,
        logger: &Logger,
    ) {
        let update_height = response.height();
        let current_height = self.routing_height;
        if update_height <= self.routing_height {
            warn!(
                logger,
                "routing returned invalid height {update_height} while at {current_height}",
            );
            return;
        }
        let routing_protos = match response.routings() {
            Ok(v) => v,
            Err(err) => {
                warn!(logger, "error decoding routing {err:?}");
                return;
            }
        };
        let mut proto_stream = tokio_stream::iter(routing_protos.iter());
        while let Some(proto) = proto_stream.next().await {
            match Routing::from_proto(logger, proto) {
                Ok(routing) => {
                    self.handle_oui_routing_update(gateway, &routing, shutdown, logger)
                        .await
                }
                Err(err) => warn!(logger, "failed to parse routing: {err:?}"),
            }
        }
        self.routing_height = update_height;
        info!(logger, "updated routing to height {:?}", update_height)
    }

    #[allow(clippy::map_entry)]
    async fn handle_oui_routing_update(
        &mut self,
        gateway: &mut GatewayService,
        routing: &Routing,
        shutdown: &triggered::Listener,
        logger: &Logger,
    ) {
        let mut uris = tokio_stream::iter(routing.uris.iter());
        while let Some(uri) = uris.next().await {
            let key = RouterKey {
                oui: routing.oui,
                uri: uri.to_owned(),
            };
            // We have to allow clippy::map_entry above since we need to borrow
            // immutable before borrowing as mutable to insert
            if !self.routers.contains_key(&key) {
                match self
                    .start_router(gateway, shutdown.clone(), routing.clone(), uri.clone())
                    .await
                {
                    Ok(router_entry) => {
                        self.routers.insert(key, router_entry);
                    }
                    Err(err) => {
                        warn!(logger, "faild to construct router: {err:?}");
                    }
                }
            }
        }
        // Remove any routers that are not in the new oui uri list
        let mut removables = Vec::with_capacity(self.routers.len());
        self.routers.retain(|key, entry| {
            if key.oui == routing.oui && !entry.routing.contains_uri(&key.uri) {
                // Router will be removed from the map. The router is expected
                // to stop itself when it receives the stop message
                info!(logger, "removing router";
                    "oui" => key.oui,
                    "uri" => key.uri.uri.to_string()
                );
                removables.push(entry.dispatch.clone());
                return false;
            }
            true
        });
        for removable in removables {
            removable.stop().await;
        }
    }

    async fn start_router(
        &self,
        gateway: &mut GatewayService,
        shutdown: triggered::Listener,
        routing: Routing,
        uri: KeyedUri,
    ) -> Result<RouterEntry> {
        // We start the router scope at the root logger to avoid picking up the
        // previously set KV pairs (which causes dupes)
        let logger = slog_scope::logger();
        let (client_tx, client_rx) = router::client::message_channel(10);
        let mut client = RouterClient::new(
            routing.oui,
            self.region,
            uri,
            gateway.clone(),
            self.downlinks.clone(),
            self.keypair.clone(),
            self.cache_settings.clone(),
        )
        .await?;
        let join_handle =
            tokio::spawn(async move { client.run(client_rx, shutdown, &logger).await });
        Ok(RouterEntry {
            routing,
            dispatch: client_tx,
            join_handle,
        })
    }
}

impl std::future::Future for RouterEntry {
    type Output = std::result::Result<Result, tokio::task::JoinError>;

    fn poll(
        mut self: Pin<&mut Self>,
        cxt: &mut Context<'_>,
    ) -> Poll<<Self as futures::Future>::Output> {
        Pin::new(&mut self.join_handle).poll(cxt)
    }
}
