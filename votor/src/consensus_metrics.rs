#![allow(dead_code)]

use {
    agave_votor_messages::vote::Vote,
    histogram::Histogram,
    solana_clock::{Epoch, Slot},
    solana_metrics::datapoint_info,
    solana_pubkey::Pubkey,
    std::{
        collections::BTreeMap,
        time::{Duration, Instant},
    },
};

/// Returns a [`Histogram`] configured for the use cases for this module.
///
/// Keeps the default precision and reduces the max value to 10s to get finer grained resolution.
fn build_histogram() -> Histogram {
    Histogram::configure()
        .max_value(10_000_000)
        .build()
        .unwrap()
}

/// Tracks all [`Vote`] metrics for a given node.
#[derive(Debug)]
struct NodeVoteMetrics {
    notar: Histogram,
    notar_fallback: Histogram,
    skip: Histogram,
    skip_fallback: Histogram,
    final_: Histogram,
}

impl Default for NodeVoteMetrics {
    fn default() -> Self {
        let histogram = build_histogram();
        Self {
            notar: histogram.clone(),
            notar_fallback: histogram.clone(),
            skip: histogram.clone(),
            skip_fallback: histogram.clone(),
            final_: histogram,
        }
    }
}

impl NodeVoteMetrics {
    /// Records metrics for when `vote` was received after `elapsed` time has passed since the start of the slot.
    fn record_vote(&mut self, vote: &Vote, elapsed: Duration) {
        let elapsed = elapsed.as_micros();
        let elapsed = match elapsed.try_into() {
            Ok(e) => e,
            Err(err) => {
                warn!(
                    "recording duration {elapsed} for vote {vote:?}: conversion to u64 failed \
                     with {err}"
                );
                return;
            }
        };
        let res = match vote {
            Vote::Notarize(_) => self.notar.increment(elapsed),
            Vote::NotarizeFallback(_) => self.notar_fallback.increment(elapsed),
            Vote::Skip(_) => self.skip.increment(elapsed),
            Vote::SkipFallback(_) => self.skip_fallback.increment(elapsed),
            Vote::Finalize(_) => self.final_.increment(elapsed),
        };
        match res {
            Ok(()) => (),
            Err(err) => {
                warn!(
                    "recording duration {elapsed} for vote {vote:?}: recording failed with {err}"
                );
            }
        }
    }
}

/// Errors returned from [`AgMetrics::record_vote`].
#[derive(Debug)]
pub enum RecordVoteError {
    /// Could not find start of slot entry.
    SlotNotFound,
}

/// Errors returned from [`AgMetrics::record_block_hash_seen`].
#[derive(Debug)]
pub enum RecordBlockHashError {
    /// Could not find start of slot entry.
    SlotNotFound,
}

/// Tracks various Consensus related metrics.
pub struct ConsensusMetrics {
    /// Used to track this node's view of how the other nodes on the network are voting.
    node_metrics: BTreeMap<Pubkey, NodeVoteMetrics>,
    /// Used to track when this node received blocks from different leaders in the network.
    leader_metrics: BTreeMap<Pubkey, Histogram>,
    /// Tracks when individual slots began.
    ///
    /// Relies on [`TimerManager`] to notify of start of slots.
    /// The manager uses parent ready event and timeouts as per the Alpenglow protocol to determine start of slots.
    start_of_slot: BTreeMap<Slot, Instant>,
    /// Tracks the current epoch, used for end of epoch reporting.
    current_epoch: Epoch,
}

impl ConsensusMetrics {
    pub fn new(epoch: Epoch) -> Self {
        Self {
            node_metrics: BTreeMap::default(),
            leader_metrics: BTreeMap::default(),
            start_of_slot: BTreeMap::default(),
            current_epoch: epoch,
        }
    }

    /// Records a `vote` from the node with `id`.
    pub fn record_vote(&mut self, id: Pubkey, vote: &Vote) -> Result<(), RecordVoteError> {
        let Some(start) = self.start_of_slot.get(&vote.slot()) else {
            return Err(RecordVoteError::SlotNotFound);
        };
        let node = self.node_metrics.entry(id).or_default();
        let elapsed = start.elapsed();
        node.record_vote(vote, elapsed);
        Ok(())
    }

    /// Records when a block for `slot` was seen and the `leader` is responsible for producing it.
    pub fn record_block_hash_seen(
        &mut self,
        leader: Pubkey,
        slot: Slot,
    ) -> Result<(), RecordBlockHashError> {
        let Some(start) = self.start_of_slot.get(&slot) else {
            return Err(RecordBlockHashError::SlotNotFound);
        };
        let elapsed = start.elapsed().as_micros();
        let elapsed = match elapsed.try_into() {
            Ok(e) => e,
            Err(err) => {
                warn!(
                    "recording duration {elapsed} for block hash for slot {slot}: conversion to \
                     u64 failed with {err}"
                );
                return Ok(());
            }
        };
        let histogram = self
            .leader_metrics
            .entry(leader)
            .or_insert_with(build_histogram);
        match histogram.increment(elapsed) {
            Ok(()) => (),
            Err(err) => {
                warn!(
                    "recording duration {elapsed} for block hash for slot {slot}: recording \
                     failed with {err}"
                );
            }
        }
        Ok(())
    }

    /// Records when a given slot started.
    pub fn record_start_of_slot(&mut self, slot: Slot) {
        self.start_of_slot.entry(slot).or_insert(Instant::now());
    }

    /// Performs end of epoch reporting and reset all the statistics for the subsequent epoch.
    fn end_of_epoch_reporting(&mut self) {
        for (addr, metrics) in &self.node_metrics {
            let addr = addr.to_string();
            datapoint_info!("votor_consensus_metrics",
                "address" => addr,
                ("notar_vote_count", metrics.notar.entries(), i64),
                ("notar_vote_mean", metrics.notar.mean().ok(), Option<i64>),
                ("notar_vote_stddev", metrics.notar.stddev(), Option<i64>),
                ("notar_vote_maximum", metrics.notar.maximum().ok(), Option<i64>),

                ("notar_fallback_vote_count", metrics.notar_fallback.entries(), i64),
                ("notar_fallback_vote_mean", metrics.notar_fallback.mean().ok(), Option<i64>),
                ("notar_fallback_vote_stddev", metrics.notar_fallback.stddev(), Option<i64>),
                ("notar_fallback_vote_maximum", metrics.notar_fallback.maximum().ok(), Option<i64>),

                ("skip_vote_count", metrics.skip.entries(), i64),
                ("skip_vote_mean", metrics.skip.mean().ok(), Option<i64>),
                ("skip_vote_stddev", metrics.skip.stddev(), Option<i64>),
                ("skip_vote_maximum", metrics.skip.maximum().ok(), Option<i64>),

                ("skip_fallback_vote_count", metrics.skip_fallback.entries(), i64),
                ("skip_fallback_vote_mean", metrics.skip_fallback.mean().ok(), Option<i64>),
                ("skip_fallback_vote_stddev", metrics.skip_fallback.stddev(), Option<i64>),
                ("skip_fallback_vote_maximum", metrics.skip_fallback.maximum().ok(), Option<i64>),

                ("finalize_vote_count", metrics.final_.entries(), i64),
                ("finalize_vote_mean", metrics.final_.mean().ok(), Option<i64>),
                ("finalize_vote_stddev", metrics.final_.stddev(), Option<i64>),
                ("finalize_vote_maximum", metrics.final_.maximum().ok(), Option<i64>),
            );
        }

        for (addr, histogram) in &self.leader_metrics {
            let addr = addr.to_string();
            datapoint_info!("votor_consensus_metrics",
                "address" => addr,
                ("blocks_seen_vote_count", histogram.entries(), i64),
                ("blocks_seen_vote_mean", histogram.mean().ok(), Option<i64>),
                ("blocks_seen_vote_stddev", histogram.stddev(), Option<i64>),
                ("blocks_seen_vote_maximum", histogram.maximum().ok(), Option<i64>),
            );
        }

        self.node_metrics.clear();
        self.leader_metrics.clear();
        self.start_of_slot.clear();
    }

    /// This function can be called if there is a new [`Epoch`] and it will carry out end of epoch reporting.
    pub fn maybe_new_epoch(&mut self, epoch: Epoch) {
        assert!(epoch >= self.current_epoch);
        if epoch != self.current_epoch {
            self.current_epoch = epoch;
            self.end_of_epoch_reporting();
        }
    }
}
