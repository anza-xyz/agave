/// Implements the [`crate::shred::traits::Shred`] accessors which are identical for every shred
/// type. Usable both by finished shreds and by shred builders, since it only requires a `payload`
/// field which derefs to `[u8]`.
macro_rules! impl_shred_common_read {
    () => {
        #[inline]
        fn common_header(&self) -> &ShredCommonHeader {
            &self.common_header
        }

        #[inline]
        fn payload_bytes(&self) -> &[u8] {
            &self.payload
        }
    };
}

/// Implements the [`crate::shred::traits::ShredWithPayload`] accessors which
/// are identical for every shred type.
macro_rules! impl_shred_common_payload {
    () => {
        #[inline]
        fn payload(&self) -> &Payload {
            &self.payload
        }

        #[inline]
        fn into_payload(self) -> Payload {
            self.payload
        }
    };
}

pub(super) use {impl_shred_common_payload, impl_shred_common_read};
