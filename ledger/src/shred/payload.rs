use {
    bytes::{Bytes, BytesMut},
    std::{
        ops::{Bound, Deref, DerefMut, RangeBounds, RangeFull},
        slice::SliceIndex,
    },
};

#[derive(Clone, Debug, Eq)]
pub struct Payload {
    pub bytes: Bytes,
}

impl Payload {
    #[inline]
    pub fn into_bytes_mut(self) -> BytesMut {
        self.bytes.into()
    }

    #[inline]
    pub fn as_mut(&mut self) -> PayloadMutGuard<'_, RangeFull> {
        PayloadMutGuard::new(self, ..)
    }

    #[inline]
    pub fn get_mut<I>(&mut self, index: I) -> Option<PayloadMutGuard<'_, I>>
    where
        I: RangeBounds<usize>,
    {
        match index.end_bound() {
            Bound::Included(&end) if end >= self.bytes.len() => None,
            Bound::Excluded(&end) if end > self.bytes.len() => None,
            _ => Some(PayloadMutGuard::new(self, index)),
        }
    }

    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.bytes.truncate(len);
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
        Self {
            bytes: Bytes::from(bytes),
        }
    }
}

impl From<Bytes> for Payload {
    #[inline]
    fn from(bytes: Bytes) -> Self {
        Self { bytes }
    }
}

impl From<BytesMut> for Payload {
    #[inline]
    fn from(bytes: BytesMut) -> Self {
        Self {
            bytes: bytes.freeze(),
        }
    }
}

impl AsRef<[u8]> for Payload {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

impl Deref for Payload {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.bytes.deref()
    }
}

pub struct PayloadMutGuard<'a, I> {
    payload: &'a mut Payload,
    bytes_mut: BytesMut,
    slice_index: I,
}

impl<'a, I> PayloadMutGuard<'a, I> {
    #[inline]
    pub fn new(payload: &'a mut Payload, slice_index: I) -> Self {
        let bytes_mut: BytesMut = std::mem::take(&mut payload.bytes).into();
        Self {
            payload,
            bytes_mut,
            slice_index,
        }
    }
}

impl<I> Drop for PayloadMutGuard<'_, I> {
    #[inline]
    fn drop(&mut self) {
        self.payload.bytes = std::mem::take(&mut self.bytes_mut).freeze();
    }
}

impl<I> Deref for PayloadMutGuard<'_, I>
where
    I: SliceIndex<[u8]> + Clone,
{
    type Target = <I as SliceIndex<[u8]>>::Output;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.bytes_mut[self.slice_index.clone()]
    }
}

impl<I> DerefMut for PayloadMutGuard<'_, I>
where
    I: SliceIndex<[u8]> + Clone,
{
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.bytes_mut[self.slice_index.clone()]
    }
}

impl<I> AsMut<[u8]> for PayloadMutGuard<'_, I>
where
    I: SliceIndex<[u8], Output = [u8]> + Clone,
{
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.bytes_mut[self.slice_index.clone()]
    }
}

impl<I> AsRef<[u8]> for PayloadMutGuard<'_, I>
where
    I: SliceIndex<[u8], Output = [u8]> + Clone,
{
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.bytes_mut[self.slice_index.clone()]
    }
}

#[cfg(test)]
mod test {
    use super::Payload;

    #[test]
    fn test_guard_write_back() {
        let mut payload = Payload::from(vec![1, 2, 3, 4, 5]);
        {
            let mut guard = payload.get_mut(..).unwrap();
            assert_eq!(guard[0], 1);
            assert_eq!(guard[1], 2);
            guard[0] = 10;
            guard[1] = 20;
            assert_eq!(guard[0], 10);
            assert_eq!(guard[1], 20);
        }

        assert_eq!(payload.bytes[..], vec![10, 20, 3, 4, 5]);
    }
}
