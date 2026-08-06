#[derive(Default)]
pub(super) struct BlockIdRepairResponsesStats {
    /// Total # of received response packets
    pub total_packets: usize,

    /// Response packets that were valid and processed
    pub processed: usize,

    /// Dropped due to throttling responses
    pub dropped_packets: usize,

    /// Packets that failed deserialization (malformed, no nonce, etc).
    pub invalid_packets: usize,
    /// Packets that deserialized OK but carried a nonce we are not tracking
    /// (never sent, already consumed, or expired).
    pub unknown_nonce_responses: usize,
    /// ParentFecSetCount responses for an active nonce whose Merkle proof
    /// failed verification — possible adversarial peer.
    pub verify_failed_parent_fec_set_count: usize,
    /// FecSetRoot responses for an active nonce whose Merkle proof failed
    /// verification — possible adversarial peer.
    pub verify_failed_fec_set_root: usize,

    /// Ping challenges
    pub ping_responses: usize,
    /// Ping challenges from peers we have not recently requested metadata from.
    pub unexpected_ping_responses: usize,
    /// Pong responses dropped because the send failed.
    pub dropped_pong_responses: usize,
    /// Pong responses sent for ping challenges.
    pub sent_pong_responses: usize,

    /// Responses to ParentAndFecSetCount requests
    pub parent_fec_set_count_responses: usize,
    /// Responses to FecSetRoot requests
    pub fec_set_root_responses: usize,
}

impl BlockIdRepairResponsesStats {
    pub fn report(&mut self) {
        datapoint_info!(
            "block_id_repair_responses",
            ("total_packets", self.total_packets, i64),
            ("processed", self.processed, i64),
            ("dropped_packets", self.dropped_packets, i64),
            ("invalid_packets", self.invalid_packets, i64),
            (
                "unknown_nonce_responses",
                self.unknown_nonce_responses,
                i64
            ),
            (
                "verify_failed_parent_fec_set_count",
                self.verify_failed_parent_fec_set_count,
                i64
            ),
            (
                "verify_failed_fec_set_root",
                self.verify_failed_fec_set_root,
                i64
            ),
            ("ping_responses", self.ping_responses, i64),
            (
                "unexpected_ping_responses",
                self.unexpected_ping_responses,
                i64
            ),
            ("dropped_pong_responses", self.dropped_pong_responses, i64),
            ("sent_pong_responses", self.sent_pong_responses, i64),
            (
                "parent_fec_set_count_responses",
                self.parent_fec_set_count_responses,
                i64
            ),
            ("fec_set_root_responses", self.fec_set_root_responses, i64),
        );
        *self = Self::default();
    }
}

#[derive(Default)]
pub(super) struct BlockIdRepairRequestsStats {
    /// Total requests we sent
    pub total_requests: usize,

    pub parent_fec_set_count_requests: usize,
    pub fec_set_root_requests: usize,
    pub shred_for_block_id_requests: usize,
}

impl BlockIdRepairRequestsStats {
    pub fn report(&mut self) {
        datapoint_info!(
            "block_id_repair_requests",
            ("total_requests", self.total_requests, i64),
            (
                "parent_fec_set_count_requests",
                self.parent_fec_set_count_requests,
                i64
            ),
            ("fec_set_root_requests", self.fec_set_root_requests, i64),
            (
                "shred_for_block_id_requests",
                self.shred_for_block_id_requests,
                i64
            ),
        );
        *self = Self::default();
    }
}
