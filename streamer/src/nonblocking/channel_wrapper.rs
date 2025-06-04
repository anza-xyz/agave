use crossbeam_channel::{Sender, Receiver, unbounded};

pub trait MyChannelSender<T>: Send + Sync + 'static {
    fn send(&self, value: T) -> Result<(), crossbeam_channel::SendError<T>>;
    fn try_send(&self, value: T) -> Result<(), crossbeam_channel::TrySendError<T>>;
}

pub struct MyChannelSendWrapper<T> {
    sender: Sender<T>,
}

impl<T: Send> MyChannelSendWrapper<T> {
    pub fn new(sender: Sender<T>) -> Self {
        MyChannelSendWrapper { sender }
    }
}

impl<T: Send + 'static> MyChannelSender<T> for MyChannelSendWrapper<T> {
    fn send(&self, value: T) -> Result<(), crossbeam_channel::SendError<T>> {
        self.sender.send(value)
    }

    fn try_send(&self, value: T) -> Result<(), crossbeam_channel::TrySendError<T>> {
        self.sender.try_send(value)
    }
}
