use {
    crate::{Signature, SIGNATURE_BYTES},
    serde::{ser::SerializeTuple, Serialize, Serializer},
};

impl Serialize for Signature {
    #[inline]
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut tup = serializer.serialize_tuple(SIGNATURE_BYTES)?;
        for el in self.0.iter() {
            tup.serialize_element(el)?;
        }

        tup.end()
    }
}
