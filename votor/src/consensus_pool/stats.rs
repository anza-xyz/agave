use {
    agave_votor_messages::{consensus_message::CertificateType, vote::Vote},
    solana_metrics::datapoint_info,
    std::time::{Duration, Instant},
};

const STATS_REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Struct to hold stats for different certificate types.
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
    fn increment(&mut self, cert_type: &CertificateType) {
        match cert_type {
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

    /// Reports the certificate related statistics.
    fn report(&self, header: &'static str) {
        let Self {
            finalize,
            finalize_fast,
            notarize,
            notarize_fallback,
            skip,
            genesis,
        } = *self;
        datapoint_info!(
            header,
            ("finalize", finalize, i64),
            ("finalize_fast", finalize_fast, i64),
            ("notarize", notarize, i64),
            ("notarize_fallback", notarize_fallback, i64),
            ("skip", skip, i64),
            ("genesis", genesis, i64),
        )
    }
}

/// Struct to hold stats for different vote types.
#[derive(Default)]
struct VoteStats {
    notarize: u64,
    finalize: u64,
    skip: u64,
    notarize_fallback: u64,
    skip_fallback: u64,
    genesis: u64,
}

impl VoteStats {
    /// Increments the stats associated with the votes by one.
    fn increment(&mut self, vote: &Vote) {
        match vote {
            Vote::Notarize(_) => self.notarize = self.notarize.saturating_add(1),
            Vote::NotarizeFallback(_) => {
                self.notarize_fallback = self.notarize_fallback.saturating_add(1)
            }
            Vote::Skip(_) => self.skip = self.skip.saturating_add(1),
            Vote::SkipFallback(_) => self.skip_fallback = self.skip_fallback.saturating_add(1),
            Vote::Finalize(_) => self.finalize = self.finalize.saturating_add(1),
            Vote::Genesis(_) => self.genesis = self.genesis.saturating_add(1),
        }
    }

    /// Reports the vote related statistics.
    fn report(&self) {
        let Self {
            finalize,
            notarize,
            notarize_fallback,
            skip,
            skip_fallback,
            genesis,
        } = *self;
        datapoint_info!(
            "consensus_ingested_votes",
            ("finalize", finalize, i64),
            ("notarize", notarize, i64),
            ("notarize_fallback", notarize_fallback, i64),
            ("skip", skip, i64),
            ("skip_fallback", skip_fallback, i64),
            ("genesis", genesis, i64),
        )
    }
}

pub(crate) struct ConsensusPoolStats {
    pub(crate) invalid_votes: u32,
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
    ingested_votes: VoteStats,
    pub(crate) last_request_time: Instant,
}

impl Default for ConsensusPoolStats {
    fn default() -> Self {
        Self {
            invalid_votes: 0,
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
            ingested_votes: VoteStats::default(),
            last_request_time: Instant::now(),
        }
    }
}

impl ConsensusPoolStats {
    pub fn incr_ingested_vote(&mut self, vote: &Vote) {
        self.ingested_votes.increment(vote);
    }

    pub fn incr_cert_type(&mut self, cert_type: &CertificateType, is_generated: bool) {
        if is_generated {
            self.new_certs_generated.increment(cert_type);
        } else {
            self.new_certs_ingested.increment(cert_type);
        };
    }

    fn report(&self) {
        let Self {
            invalid_votes,
            event_safe_to_skip,
            event_safe_to_notarize,
            exist_votes,
            exist_certs,
            incoming_votes,
            incoming_certs,
            out_of_range_votes,
            out_of_range_certs,
            ingested_votes,
            new_certs_generated,
            new_certs_ingested,
            last_request_time: _,
        } = self;
        datapoint_info!(
            "consensus_pool_stats",
            ("vote_pool_invalid_votes", *invalid_votes as i64, i64),
            ("event_safe_to_skip", *event_safe_to_skip as i64, i64),
            (
                "event_safe_to_notarize",
                *event_safe_to_notarize as i64,
                i64
            ),
            ("exist_votes", *exist_votes as i64, i64),
            ("exist_certs", *exist_certs as i64, i64),
            ("incoming_votes", *incoming_votes as i64, i64),
            ("incoming_certs", *incoming_certs as i64, i64),
            ("out_of_range_votes", *out_of_range_votes as i64, i64),
            ("out_of_range_certs", *out_of_range_certs as i64, i64),
        );

        ingested_votes.report();
        new_certs_generated.report("consensus_pool_generated_certs");
        new_certs_ingested.report("consensus_pool_ingested_certs");
    }

    pub fn maybe_report(&mut self) {
        if self.last_request_time.elapsed() >= STATS_REPORT_INTERVAL {
            self.report();
            *self = Self::default();
        }
    }
}
