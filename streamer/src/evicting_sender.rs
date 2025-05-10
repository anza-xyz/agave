use {
    crate::streamer::ChannelSend,
    crossbeam_channel::{bounded, Receiver, SendError, Sender, TryRecvError, TrySendError},
};

/// A sender implementation that evicts the oldest message when the channel is full.
#[derive(Clone)]
pub struct EvictingSender<T> {
    sender: Sender<T>,
    receiver: Receiver<T>,
}

impl<T> EvictingSender<T> {
    /// Create a new evicting sender with provided sender, receiver.
    #[inline]
    pub fn new(sender: Sender<T>, receiver: Receiver<T>) -> Self {
        Self { sender, receiver }
    }

    /// Create a new `EvictingSender` with a bounded channel of the specified capacity.
    #[inline]
    pub fn new_bounded(capacity: usize) -> (Self, Receiver<T>) {
        let (sender, receiver) = bounded(capacity);
        (Self::new(sender, receiver.clone()), receiver)
    }
}

impl<T> ChannelSend<T> for EvictingSender<T>
where
    T: Send + 'static,
{
    #[inline]
    fn send(&self, msg: T) -> std::result::Result<(), SendError<T>> {
        self.sender.send(msg)
    }

    fn try_send(&self, msg: T) -> std::result::Result<(), TrySendError<T>> {
        let Err(e) = self.sender.try_send(msg) else {
            return Ok(());
        };

        match e {
            // Prefer newer messages over older messages.
            TrySendError::Full(msg) => match self.receiver.try_recv() {
                Ok(older) => {
                    // Attempt to requeue the newer message.
                    // NB: if multiple senders are used, and another sender is faster than us to send() after we've popped `older`,
                    // our try_send() will fail with Full(msg), in which case we drop the new message.
                    self.sender.try_send(msg)?;
                    // Propagate the error _with the older message_.
                    Err(TrySendError::Full(older))
                }
                // Unlikely race condition -- it was just indicated that the channel is full.
                // Attempt to requeue the message.
                Err(TryRecvError::Empty) => self.sender.try_send(msg),
                // Unreachable in practice since we maintain a reference to both the sender and receiver.
                Err(TryRecvError::Disconnected) => unreachable!(),
            },
            // Unreachable in practice since we maintain a reference to both the sender and receiver.
            TrySendError::Disconnected(_) => unreachable!(),
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }

    #[inline]
    fn len(&self) -> usize {
        self.receiver.len()
    }
}
