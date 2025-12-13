macro_rules! impl_shred_common {
    () => {
        #[inline]
        fn common_header(&self) -> &ShredCommonHeader {
            &self.common_header
        }

        #[inline]
        fn payload(&self) -> &Payload {
            &self.payload
        }

        #[inline]
        fn into_payload(self) -> Payload {
            self.payload
        }

        #[inline]
        fn set_signature(&mut self, signature: Signature) {
            self.payload.as_mut()[..SIZE_OF_SIGNATURE].copy_from_slice(signature.as_ref());
            self.common_header.signature = signature;
        }
    };
}

pub(super) use impl_shred_common;
