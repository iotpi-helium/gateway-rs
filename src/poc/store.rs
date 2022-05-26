use crate::{poc::PocId, service::gateway::Challenge, CacheSettings, KeyedUri, Result};
use helium_proto::{gateway_poc_report_req_v1, GatewayPocReportReqV1};
use std::{
    collections::{hash_map::IterMut, HashMap},
    time::Instant,
};

pub struct PocStore {
    challenges: HashMap<PocId, QueueChallenge>,
    reports: HashMap<PocId, QueueReport>,
}
#[derive(Debug)]
pub struct QueueChallenge {
    pub(crate) challenge: Challenge,
    pub(crate) received: Instant,
}

impl PartialEq for QueueChallenge {
    fn eq(&self, other: &Self) -> bool {
        self.challenge.poc_id == other.challenge.poc_id
    }
}

impl Eq for QueueChallenge {}

impl From<Challenge> for QueueChallenge {
    fn from(challenge: Challenge) -> Self {
        let received = Instant::now();
        Self {
            received,
            challenge,
        }
    }
}

#[derive(Debug)]
pub struct QueueReport {
    pub(crate) received: Instant,
    pub(crate) challenger: Option<KeyedUri>,
    pub(crate) report: GatewayPocReportReqV1,
    pub(crate) retry_count: i8,
}

impl PartialEq for QueueReport {
    fn eq(&self, other: &Self) -> bool {
        self.received == other.received
    }
}

impl Eq for QueueReport {}

impl PartialOrd for QueueReport {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.received.partial_cmp(&other.received)
    }
}

impl From<GatewayPocReportReqV1> for QueueReport {
    fn from(v: GatewayPocReportReqV1) -> Self {
        Self {
            received: Instant::now(),
            challenger: None,
            report: v,
            retry_count: 0,
        }
    }
}

impl QueueReport {
    pub fn report_type(&self) -> &'static str {
        match self.report.msg {
            None => "unknown",
            Some(gateway_poc_report_req_v1::Msg::Witness(_)) => "witness",
            Some(gateway_poc_report_req_v1::Msg::Receipt(_)) => "receipt",
        }
    }
}

impl PocStore {
    pub fn new(settings: &CacheSettings) -> Self {
        let challenges = HashMap::new();
        let reports = HashMap::new();
        Self {
            challenges,
            reports,
        }
    }

    // Challenge cache
    pub fn store_waiting_challenge<T: Into<QueueChallenge>>(
        &mut self,
        poc_id: PocId,
        challenge: T,
    ) -> Result {
        self.challenges.insert(poc_id, challenge.into());
        Ok(())
    }

    pub fn remove_waiting_challenge(&mut self, poc_id: &PocId) -> Option<QueueChallenge> {
        self.challenges.remove(poc_id)
    }

    pub fn waiting_challenges_mut(&mut self) -> IterMut<'_, PocId, QueueChallenge> {
        self.challenges.iter_mut()
    }

    // Witness/Receipt report cache

    pub fn store_waiting_report<T: Into<QueueReport>>(
        &mut self,
        poc_id: PocId,
        report: T,
    ) -> Result {
        self.reports.insert(poc_id, report.into());
        Ok(())
    }

    pub fn get_waiting_report_mut(&mut self, poc_id: &PocId) -> Option<&mut QueueReport> {
        self.reports.get_mut(poc_id)
    }

    pub fn remove_waiting_report(&mut self, poc_id: &PocId) -> Option<QueueReport> {
        self.reports.remove(poc_id)
    }

    pub fn waiting_reports_mut(&mut self) -> IterMut<'_, PocId, QueueReport> {
        self.reports.iter_mut()
    }

    pub fn gc_waiting_reports(&mut self, max_retry_count: i8) {
        self.reports
            .retain(|_, report| report.retry_count >= 0 && report.retry_count < max_retry_count);
    }
}
