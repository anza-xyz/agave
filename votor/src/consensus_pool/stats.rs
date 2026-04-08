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

    fn record(&self, header: &'static str) {
        let Self {
            finalize,
            finalize_fast,
            notarize,
            notarize_fallback,
            skip,
            genesis,
        } = self;
        datapoint_info!(
            header,
            ("finalize", *finalize, i64),
            ("finalize_fast", *finalize_fast, i64),
            ("notarize", *notarize, i64),
            ("notarize_fallback", *notarize_fallback, i64),
            ("skip", *skip, i64),
            ("genesis", *genesis, i64),
        )
    }
}

#[derive(Default)]
struct VoteStats {
    finalize: u64,
    notarize: u64,
    notar_fallback: u64,
    skip: u64,
    skip_fallback: u64,
    genesis: u64,
}

impl VoteStats {
    fn increment(&mut self, vote_type: &VoteType) {
        match vote_type {
            VoteType::Finalize => self.finalize = self.finalize.saturating_add(1),
            VoteType::Notarize => self.notarize = self.notarize.saturating_add(1),
            VoteType::NotarizeFallback => {
                self.notar_fallback = self.notar_fallback.saturating_add(1)
            }
            VoteType::Skip => self.skip = self.skip.saturating_add(1),
            VoteType::SkipFallback => self.skip_fallback = self.skip_fallback.saturating_add(1),
            VoteType::Genesis => self.genesis = self.genesis.saturating_add(1),
        }
    }

    fn record(&self, header: &'static str) {
        let Self {
            finalize,
            notarize,
            notar_fallback,
            skip,
            skip_fallback,
            genesis,
        } = self;
        datapoint_info!(
            header,
            ("finalize", *finalize, i64),
            ("notarize", *notarize, i64),
            ("notar_fallback", *notar_fallback, i64),
            ("skip", *skip, i64),
            ("skip_fallback", *skip_fallback, i64),
            ("genesis", *genesis, i64),
        )
    }
}

pub(super) struct ConsensusPoolStats {
    pub(super) conflicting_votes: u32,
    pub(super) event_safe_to_notarize: u32,
    pub(super) event_safe_to_skip: u32,
    pub(super) exist_certs: u32,
    pub(super) exist_votes: u32,
    pub(super) incoming_certs: u32,
    pub(super) incoming_votes: u32,
    pub(super) out_of_range_certs: u32,
    pub(super) out_of_range_votes: u32,

    certs_generated: CertificateStats,
    certs_ingested: CertificateStats,
    ingested_votes: VoteStats,

    pub(super) last_request_time: Instant,
}

impl Default for ConsensusPoolStats {
    fn default() -> Self {
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

            certs_ingested: CertificateStats::default(),
            certs_generated: CertificateStats::default(),
            ingested_votes: VoteStats::default(),

            last_request_time: Instant::now(),
        }
    }
}

impl ConsensusPoolStats {
    pub(super) fn incr_ingested_vote_type(&mut self, vote_type: VoteType) {
        self.ingested_votes.increment(&vote_type);
    }

    pub(super) fn incr_generated_cert(&mut self, cert_type: &CertificateType) {
        self.certs_generated.increment(cert_type);
    }

    pub(super) fn incr_ingested_cert(&mut self, cert_type: &CertificateType) {
        self.certs_ingested.increment(cert_type);
    }

    /// Reports the certificate related statistics.
    fn report(&self) {
        let Self {
            conflicting_votes,
            event_safe_to_notarize,
            event_safe_to_skip,
            exist_certs,
            exist_votes,
            incoming_certs,
            incoming_votes,
            ingested_votes,
            certs_generated,
            certs_ingested,
            out_of_range_certs,
            out_of_range_votes,
            last_request_time: _,
        } = self;
        datapoint_info!(
            "consensus_pool",
            ("conflicting_votes", *conflicting_votes as i64, i64),
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
        ingested_votes.record("consensus_pool_ingested_votes");
        certs_ingested.record("consensus_pool_ingested_certs");
        certs_generated.record("consensus_pool_generated_certs");
    }

    pub(super) fn maybe_report(&mut self) {
        if self.last_request_time.elapsed() >= STATS_REPORT_INTERVAL {
            self.report();
            *self = Self::default();
        }
    }
}
