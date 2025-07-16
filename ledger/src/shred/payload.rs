use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};
#[cfg(any(test, feature = "dev-context-only-utils"))]
use {
    crate::shred::Nonce,
    solana_perf::packet::{
        bytes::{BufMut, BytesMut},
        BytesPacket, Meta, Packet,
    },
    std::mem,
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

#[cfg(any(test, feature = "dev-context-only-utils"))]
impl Payload {
    pub fn copy_to_packet(&self, packet: &mut Packet) {
        let size = self.len();
        packet.buffer_mut()[..size].copy_from_slice(&self[..]);
        packet.meta_mut().size = size;
    }

    pub fn to_packet(&self, nonce: Option<Nonce>) -> Packet {
        let mut packet = Packet::default();
        let size = self.len();
        packet.buffer_mut()[..size].copy_from_slice(self);
        let size = if let Some(nonce) = nonce {
            let full_size = size + mem::size_of::<Nonce>();
            packet.buffer_mut()[size..full_size].copy_from_slice(&nonce.to_le_bytes());
            full_size
        } else {
            size
        };
        packet.meta_mut().size = size;
        packet
    }

    pub fn to_bytes_packet(&self, nonce: Option<Nonce>) -> BytesPacket {
        let cap = self.len() + nonce.map(|_| mem::size_of::<Nonce>()).unwrap_or(0);
        let mut buffer = BytesMut::with_capacity(cap);
        buffer.put_slice(&self[..]);
        if let Some(nonce) = nonce {
            buffer.put_u32(nonce);
        }
        BytesPacket::new(buffer.freeze(), Meta::default())
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
