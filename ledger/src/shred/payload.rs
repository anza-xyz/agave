use std::ops::{Deref, DerefMut};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Payload(Vec<u8>);

macro_rules! dispatch {
    ($vis:vis fn $name:ident(&self $(, $arg:ident : $ty:ty)?) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(&self $(, $arg:$ty)?) $(-> $out)? {
            self.0.$name($($arg, )?)
        }
    };
}

impl Payload {
    #[inline]
    pub(super) fn make_mut(this: &mut Self) -> &mut Vec<u8> {
        &mut this.0
    }

    #[inline]
    pub fn unwrap_or_clone(this: Self) -> Vec<u8> {
        this.0
    }
}

pub(crate) mod serde_bytes_payload {
    use {
        super::*,
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

impl From<Vec<u8>> for Payload {
    #[inline]
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
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
    fn deref_mut(&mut self) -> &mut Self::Target {
        Payload::make_mut(self)
    }
}
