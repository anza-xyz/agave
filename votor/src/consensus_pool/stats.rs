use {
    agave_votor_messages::{consensus_message::CertificateType, vote::VoteType},
    solana_metrics::datapoint_info,
    std::time::{Duration, Instant},
};

const STATS_REPORT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Default)]
struct CertificateStats {
    finalize: u64,
    finalize_fast: u64,
    notarize: u64,
    notarize_fallback: u64,
    skip: u64,
    genesis: u64,
}

impl CertificateStats {
    /// Increments the stats associated with the certificate type by one.
    fn increment(&mut self, certificate: &CertificateType) {
        match certificate {
            CertificateType::Finalize(_) => self.finalize = self.finalize.saturating_add(1),
            CertificateType::FinalizeFast(_, _) => {
                self.finalize_fast = self.finalize_fast.saturating_add(1)
            }
            CertificateType::Notarize(_, _) => self.notarize = self.notarize.saturating_add(1),
            CertificateType::NotarizeFallback(_, _) => {
                self.notarize_fallback = self.notarize_fallback.saturating_add(1)
            }
            CertificateType::Skip(_) => self.skip = self.skip.saturating_add(1),
            CertificateType::Genesis(_, _) => self.genesis = self.genesis.saturating_add(1),
        }
    }

    fn record(&self, header: &'static str) {
        datapoint_info!(
            header,
            ("finalize", self.finalize, i64),
            ("finalize_fast", self.finalize_fast, i64),
            ("notarize", self.notarize, i64),
            ("notarize_fallback", self.notarize_fallback, i64),
            ("skip", self.skip, i64),
        )
    }
}

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

    new_certs_generated: CertificateStats,
    new_certs_ingested: CertificateStats,
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
        let num_vote_types = (VoteType::Genesis as usize).saturating_add(1);
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

            new_certs_ingested: CertificateStats::default(),
            new_certs_generated: CertificateStats::default(),
            ingested_votes: vec![0; num_vote_types],

            last_request_time: Instant::now(),
        }
    }

    pub fn incr_ingested_vote_type(&mut self, vote_type: VoteType) {
        let index = vote_type as usize;

        self.ingested_votes[index] = self.ingested_votes[index].saturating_add(1);
    }

    pub fn incr_cert_type(&mut self, certificate: &CertificateType, is_generated: bool) {
        if is_generated {
            self.new_certs_generated.increment(certificate);
        } else {
            self.new_certs_ingested.increment(certificate);
        }
    }

    /// Reports the certificate related statistics.
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

        self.new_certs_ingested
            .record("consensus_pool_ingested_certs");
        self.new_certs_generated
            .record("consensus_pool_generated_certs");
    }

    pub fn maybe_report(&mut self) {
        if self.last_request_time.elapsed() >= STATS_REPORT_INTERVAL {
            self.report();
            *self = Self::new();
        }
    }
}
