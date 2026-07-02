//! A sender that forwards messages to two underlying senders.
//!
//! Used to tap the retransmit path: verified shreds are sent to both the
//! normal Turbine retransmit channel and the mesh-layer batch channel.

use {
    crossbeam_channel::TrySendError,
    solana_streamer::streamer::ChannelSend,
};

/// A sender that sends each message to both `primary` and `secondary`.
///
/// `primary` is the normal Turbine retransmit sender.  `secondary` is the
/// mesh-layer tap sender.  Errors from `secondary` are silently ignored
/// (the mesh layer is best-effort and must never block the Turbine path).
pub struct DualSender<T, P, S>
where
    P: ChannelSend<T>,
    S: ChannelSend<T>,
{
    primary: P,
    secondary: S,
    _marker: std::marker::PhantomData<T>,
}

impl<T, P, S> DualSender<T, P, S>
where
    P: ChannelSend<T>,
    S: ChannelSend<T>,
{
    pub fn new(primary: P, secondary: S) -> Self {
        Self {
            primary,
            secondary,
            _marker: std::marker::PhantomData,
        }
    }
}

// ChannelSend for DualSender requires T: Clone so we can send to both.
impl<T, P, S> ChannelSend<T> for DualSender<T, P, S>
where
    T: Clone + Send + 'static,
    P: ChannelSend<T>,
    S: ChannelSend<T>,
{
    fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        // Best-effort send to the mesh tap — never block Turbine.
        let _ = self.secondary.try_send(msg.clone());
        // Primary send — errors propagate to the caller.
        self.primary.try_send(msg)
    }

    fn is_empty(&self) -> bool {
        self.primary.is_empty()
    }

    fn len(&self) -> usize {
        self.primary.len()
    }
}