use std::fmt;
use std::sync::Arc;
use crossbeam_channel::{Sender};
use crate::nonblocking::channel_wrapper;

pub trait MyChannelSender<T>: Clone + Send + Sync + 'static {
    fn try_send(&self, value: T) -> Result<(), TrySendError<T>>;
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TrySendError::Full(..) => "sending on a full channel".fmt(f),
            TrySendError::Disconnected(..) => "sending on a disconnected channel".fmt(f),
        }
    }
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TrySendError::Full(..) => "Full(..)".fmt(f),
            TrySendError::Disconnected(..) => "Disconnected(..)".fmt(f),
        }
    }
}

pub struct MyChannelSendWrapper<T> {
    sender: Arc<Sender<T>>,
}

impl<T: Send> MyChannelSendWrapper<T> {
    pub fn new(sender: Sender<T>) -> Self {
        MyChannelSendWrapper { sender: Arc::new(sender) }
    }
}

impl<T: Send + 'static> Clone for MyChannelSendWrapper<T> {
    fn clone(&self) -> Self {
        MyChannelSendWrapper {
            sender: Arc::clone(&self.sender),
        }

    }
}

impl<T: Send + 'static> MyChannelSender<T> for MyChannelSendWrapper<T> {
    // fn send(&self, value: T) -> Result<(), crossbeam_channel::SendError<T>> {
    //     self.sender.send(value)
    // }

    #[inline]
    fn try_send(&self, value: T) -> Result<(), channel_wrapper::TrySendError<T>> {
        self.sender.try_send(value).map_err(|err| err.into())
    }
}

impl<T> From<crossbeam_channel::TrySendError<T>> for TrySendError<T> {
    fn from(err: crossbeam_channel::TrySendError<T>) -> Self {
        match err {
            crossbeam_channel::TrySendError::Full(value) => TrySendError::Full(value),
            crossbeam_channel::TrySendError::Disconnected(value) => TrySendError::Disconnected(value),
        }
    }
}
