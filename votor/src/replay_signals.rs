//! Signals sent by votor to replay_stage.

use {
    agave_votor_messages::consensus_message::Block,
    crossbeam_channel::{Receiver, Sender},
    solana_clock::Slot,
    std::time::Instant,
};

pub type ReplayCancelSender = Sender<ReplayCancelCommand>;
pub type ReplayCancelReceiver = Receiver<ReplayCancelCommand>;

/// Command sent by votor to replay stage to cancel or resume replay of a skip-certified slot.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ReplayCancelCommand {
    /// The slot received a Skip certificate while no block in it has a NotarizeFallback-or-stronger
    /// certificate, so the block is unlikely to be finalized. Replay discards the slot's unfrozen
    /// bank and doesn't recreate it until the slot is resumed. The shreds are kept.
    ///
    /// Votor flags the slot's bank with `Bank::request_replay_cancel` and sends this before any
    /// later `Resume` for the slot, so that replay discards any unfrozen bank votor flagged when it
    /// processes this command.
    Cancel { slot: Slot, cancelled_at: Instant },
    /// The block has a NotarizeFallback-or-stronger certificate, so it may end up finalized (if it
    /// isn't already), and with it every block on its parent chain. Replay resumes the block's
    /// slot and every cancelled slot on its parent chain.
    Resume(Block),
}
