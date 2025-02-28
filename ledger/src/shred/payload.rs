use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};

#[derive(Clone, Debug, Eq)]
pub enum Payload {
    Shared(Arc<Vec<u8>>),
    Unique(Vec<u8>),
}

macro_rules! make_mut {
    ($self:ident) => {
        match $self {
            Self::Shared(bytes) => Arc::make_mut(bytes),
            Self::Unique(bytes) => bytes,
        }
    };
}

macro_rules! dispatch {
    ($vis:vis fn $name:ident(&self $(, $arg:ident : $ty:ty)?) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(&self $(, $arg:$ty)?) $(-> $out)? {
            match self {
                Self::Shared(bytes) => bytes.$name($($arg, )?),
                Self::Unique(bytes) => bytes.$name($($arg, )?),
            }
        }
    };
    ($vis:vis fn $name:ident(&mut self $(, $arg:ident : $ty:ty)*) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(&mut self $(, $arg:$ty)*) $(-> $out)? {
            make_mut!(self).$name($($arg, )*)
        }
    }
}

impl Payload {
    #[cfg(test)]
    dispatch!(pub(crate) fn push(&mut self, byte: u8));

    #[inline]
    pub(crate) fn resize(&mut self, size: usize, byte: u8) {
        if self.len() != size {
            make_mut!(self).resize(size, byte);
        }
    }

    #[inline]
    pub(crate) fn truncate(&mut self, size: usize) {
        if self.len() > size {
            make_mut!(self).truncate(size);
        }
    }

    #[inline]
    pub fn unwrap_or_clone(this: Self) -> Vec<u8> {
        match this {
            Self::Shared(bytes) => Arc::unwrap_or_clone(bytes),
            Self::Unique(bytes) => bytes,
        }
    }
}

pub(crate) mod serde_bytes_payload {
    use {
        super::Payload,
        serde::{Deserialize, Deserializer, Serializer},
        serde_bytes::ByteBuf,
    };

    pub(crate) fn serialize<S: Serializer>(
        payload: &Payload,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(payload)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Payload, D::Error>
    where
        D: Deserializer<'de>,
    {
        Deserialize::deserialize(deserializer)
            .map(ByteBuf::into_vec)
            .map(Payload::from)
    }
}

impl PartialEq for Payload {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl From<Vec<u8>> for Payload {
    #[inline]
    fn from(bytes: Vec<u8>) -> Self {
        Self::Unique(bytes)
    }
}

impl From<Arc<Vec<u8>>> for Payload {
    #[inline]
    fn from(bytes: Arc<Vec<u8>>) -> Self {
        Self::Shared(bytes)
    }
}

impl AsRef<[u8]> for Payload {
    dispatch!(fn as_ref(&self) -> &[u8]);
}

impl Deref for Payload {
    type Target = [u8];
    dispatch!(fn deref(&self) -> &Self::Target);
}

impl DerefMut for Payload {
    dispatch!(fn deref_mut(&mut self) -> &mut Self::Target);
}
