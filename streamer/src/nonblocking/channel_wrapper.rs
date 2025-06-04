use crossbeam_channel::{Sender, Receiver, unbounded};


pub struct MyChannelSendWrapper<T> {
    sender: Sender<T>,
}

impl<T> MyChannelSendWrapper<T> {
    pub fn new(sender: Sender<T>) -> Self {
        MyChannelSendWrapper { sender }
    }

    // TODO inline
    pub fn send(&self, value: T) -> Result<(), crossbeam_channel::SendError<T>> {
        self.sender.send(value)
    }

    pub fn try_send(&self, value: T) -> Result<(), crossbeam_channel::TrySendError<T>> {
        self.sender.try_send(value)
    }

}
