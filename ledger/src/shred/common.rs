macro_rules! dispatch {
    ($vis:vis fn $name:ident(&self $(, $arg:ident : $ty:ty)?) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(&self $(, $arg:$ty)?) $(-> $out)? {
            match self {
                Self::Legacy(shred) => shred.$name($($arg, )?),
                Self::Merkle(shred) => shred.$name($($arg, )?),
            }
        }
    };
    ($vis:vis fn $name:ident(self $(, $arg:ident : $ty:ty)?) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(self $(, $arg:$ty)?) $(-> $out)? {
            match self {
                Self::Legacy(shred) => shred.$name($($arg, )?),
                Self::Merkle(shred) => shred.$name($($arg, )?),
            }
        }
    };
    ($vis:vis fn $name:ident(&mut self $(, $arg:ident : $ty:ty)?) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(&mut self $(, $arg:$ty)?) $(-> $out)? {
            match self {
                Self::Legacy(shred) => shred.$name($($arg, )?),
                Self::Merkle(shred) => shred.$name($($arg, )?),
            }
        }
    }
}

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
            self.payload[..SIZE_OF_SIGNATURE].copy_from_slice(signature.as_ref());
            self.common_header.signature = signature;
        }
    };
}

pub(super) use {dispatch, impl_shred_common};
