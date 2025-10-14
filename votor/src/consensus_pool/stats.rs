#![allow(dead_code)]
// TODO(wen): remove allow(dead_code) when consensus_pool is fully integrated

use {
    crate::common::VoteType,
    agave_votor_messages::consensus_message::CertificateType,
    solana_metrics::datapoint_info,
    std::time::{Duration, Instant},
};

const STATS_REPORT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(crate) struct ConsensusPoolStats {
    pub(crate) conflicting_votes: u32,
    pub(crate) event_safe_to_notarize: u32,
    pub(crate) event_safe_to_skip: u32,
    pub(crate) exist_certs: u32,
    pub(crate) exist_votes: u32,
    pub(crate) incoming_certs: u32,
    pub(crate) incoming_votes: u32,
    pub(crate) out_of_range_certs: u32,
    pub(crate) out_of_range_votes: u32,

    pub(crate) new_certs_generated: Vec<u32>,
    pub(crate) new_certs_ingested: Vec<u32>,
    pub(crate) ingested_votes: Vec<u32>,

    pub(crate) last_request_time: Instant,
}

impl Default for ConsensusPoolStats {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsensusPoolStats {
    pub fn new() -> Self {
        let num_vote_types = (VoteType::SkipFallback as usize).saturating_add(1);
        let num_cert_types = (CertificateType::Skip as usize).saturating_add(1);
        Self {
            conflicting_votes: 0,
            event_safe_to_notarize: 0,
            event_safe_to_skip: 0,
            exist_certs: 0,
            exist_votes: 0,
            incoming_certs: 0,
            incoming_votes: 0,
            out_of_range_certs: 0,
            out_of_range_votes: 0,

            new_certs_ingested: vec![0; num_cert_types],
            new_certs_generated: vec![0; num_cert_types],
            ingested_votes: vec![0; num_vote_types],

            last_request_time: Instant::now(),
        }
    }

    pub fn incr_ingested_vote_type(&mut self, vote_type: VoteType) {
        let index = vote_type as usize;

        self.ingested_votes[index] = self.ingested_votes[index].saturating_add(1);
    }

    pub fn incr_cert_type(&mut self, cert_type: CertificateType, is_generated: bool) {
        let index = cert_type as usize;
        let array = if is_generated {
            &mut self.new_certs_generated
        } else {
            &mut self.new_certs_ingested
        };

        array[index] = array[index].saturating_add(1);
    }

    fn report(&self) {
        datapoint_info!(
            "consensus_pool_stats",
            ("conflicting_votes", self.conflicting_votes as i64, i64),
            ("event_safe_to_skip", self.event_safe_to_skip as i64, i64),
            (
                "event_safe_to_notarize",
                self.event_safe_to_notarize as i64,
                i64
            ),
            ("exist_votes", self.exist_votes as i64, i64),
            ("exist_certs", self.exist_certs as i64, i64),
            ("incoming_votes", self.incoming_votes as i64, i64),
            ("incoming_certs", self.incoming_certs as i64, i64),
            ("out_of_range_votes", self.out_of_range_votes as i64, i64),
            ("out_of_range_certs", self.out_of_range_certs as i64, i64),
        );

        datapoint_info!(
            "consensus_ingested_votes",
            (
                "finalize",
                *self
                    .ingested_votes
                    .get(VoteType::Finalize as usize)
                    .unwrap() as i64,
                i64
            ),
            (
                "notarize",
                *self
                    .ingested_votes
                    .get(VoteType::Notarize as usize)
                    .unwrap() as i64,
                i64
            ),
            (
                "notarize_fallback",
                *self
                    .ingested_votes
                    .get(VoteType::NotarizeFallback as usize)
                    .unwrap() as i64,
                i64
            ),
            (
                "skip",
                *self.ingested_votes.get(VoteType::Skip as usize).unwrap() as i64,
                i64
            ),
            (
                "skip_fallback",
                *self
                    .ingested_votes
                    .get(VoteType::SkipFallback as usize)
                    .unwrap() as i64,
                i64
            ),
        );

        datapoint_info!(
            "certfificate_pool_ingested_certs",
            (
                "finalize",
                *self
                    .new_certs_ingested
                    .get(CertificateType::Finalize as usize)
                    .unwrap() as i64,
                i64
            ),
            (
                "finalize_fast",
                *self
                    .new_certs_ingested
                    .get(CertificateType::FinalizeFast as usize)
                    .unwrap() as i64,
                i64
            ),
            (
                "notarize",
                *self
                    .new_certs_ingested
                    .get(CertificateType::Notarize as usize)
                    .unwrap() as i64,
                i64
            ),
            (
                "notarize_fallback",
                *self
                    .new_certs_ingested
                    .get(CertificateType::NotarizeFallback as usize)
                    .unwrap() as i64,
                i64
            ),
            (
                "skip",
                *self
                    .new_certs_ingested
                    .get(CertificateType::Skip as usize)
                    .unwrap() as i64,
                i64
            ),
        );

        datapoint_info!(
            "consensus_pool_generated_certs",
            (
                "finalize",
                *self
                    .new_certs_generated
                    .get(CertificateType::Finalize as usize)
                    .unwrap() as i64,
                i64
            ),
            (
                "finalize_fast",
                *self
                    .new_certs_generated
                    .get(CertificateType::FinalizeFast as usize)
                    .unwrap() as i64,
                i64
            ),
            (
                "notarize",
                *self
                    .new_certs_generated
                    .get(CertificateType::Notarize as usize)
                    .unwrap() as i64,
                i64
            ),
            (
                "notarize_fallback",
                *self
                    .new_certs_generated
                    .get(CertificateType::NotarizeFallback as usize)
                    .unwrap() as i64,
                i64
            ),
            (
                "skip",
                *self
                    .new_certs_generated
                    .get(CertificateType::Skip as usize)
                    .unwrap() as i64,
                i64
            ),
        );
    }

    pub fn maybe_report(&mut self) {
        if self.last_request_time.elapsed() >= STATS_REPORT_INTERVAL {
            self.report();
            *self = Self::new();
        }
    }
}
