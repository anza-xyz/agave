use {
    crate::{contact_info, duplicate_shred},
    crossbeam_channel::{RecvError, SendError},
    std::io,
    thiserror::Error,
};

#[derive(Error, Debug)]
pub enum GossipError {
    #[error("duplicate node instance")]
    DuplicateNodeInstance,
    #[error(transparent)]
    DuplicateShredError(#[from] duplicate_shred::Error),
    #[error(transparent)]
    InvalidContactInfo(#[from] contact_info::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    RecvError(#[from] RecvError),
    #[error("send error")]
    SendError,
    #[error("serialization error")]
    Serialize(#[from] Box<bincode::ErrorKind>),
}

impl<T> std::convert::From<SendError<T>> for GossipError {
    fn from(_e: SendError<T>) -> GossipError {
        GossipError::SendError
    }
}
