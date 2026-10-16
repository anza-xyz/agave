use {
    crossbeam_channel::{Sender, TrySendError},
    solana_pubkey::Pubkey,
    std::time::Duration,
};

// Core consensus types and constants
pub(crate) type Stake = u64;

/// Time bound assumed on network transmission delays during periods of synchrony.
pub const DELTA: Duration = Duration::from_millis(250);

/// Base leader handover timeout: Time after parent-ready that a validator would
/// see a leaders first fec set if that leader sent it at the very start of their
/// window.
pub(crate) const DELTA_TIMEOUT: Duration = Duration::from_millis(400);

/// Timeout for standstill detection mechanism.
pub(crate) const DELTA_STANDSTILL: Duration = Duration::from_millis(10_000);

/// Wrapper to do non-blocking send and drop msg if channel is full.
/// Returns:
/// - Err(channel_name) on channel disconnect.
pub(crate) fn nonblocking_send<T>(
    my_pubkey: &Pubkey,
    sender: &Sender<T>,
    msg: T,
    channel_name: &'static str,
) -> Result<(), &'static str> {
    match sender.try_send(msg) {
        Ok(()) => Ok(()),
        Err(TrySendError::Disconnected(_)) => Err(channel_name),
        Err(TrySendError::Full(_)) => {
            warn!("{my_pubkey}: channel \"{channel_name}\" is full, dropping msg");
            Ok(())
        }
    }
}

/// Wrapper to do blocking send if channel is full.  Returns Err(channel_name) on channel disconnect.
pub(crate) fn blocking_send<T>(
    my_pubkey: &Pubkey,
    sender: &Sender<T>,
    msg: T,
    channel_name: &'static str,
) -> Result<(), &'static str> {
    match sender.try_send(msg) {
        Ok(()) => Ok(()),
        Err(TrySendError::Disconnected(_)) => Err(channel_name),
        Err(TrySendError::Full(msg)) => {
            warn!("{my_pubkey}: channel \"{channel_name}\" is full, resorting to blocking send");
            sender.send(msg).map_err(|_| channel_name)
        }
    }
}
