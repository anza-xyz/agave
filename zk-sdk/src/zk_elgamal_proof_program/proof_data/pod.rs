use {
    crate::zk_elgamal_proof_program::proof_data::{errors::ProofDataError, ProofType},
    bytemuck_derive::{Pod, Zeroable},
    num_traits::{FromPrimitive, ToPrimitive},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(transparent)]
pub struct PodProofType(u8);
impl From<ProofType> for PodProofType {
    fn from(proof_type: ProofType) -> Self {
        Self(ToPrimitive::to_u8(&proof_type).unwrap())
    }
}
impl TryFrom<PodProofType> for ProofType {
    type Error = ProofDataError;

    fn try_from(pod: PodProofType) -> Result<Self, Self::Error> {
        FromPrimitive::from_u8(pod.0).ok_or(Self::Error::InvalidProofType)
    }
}

macro_rules! impl_wasm_to_bytes {
    (TYPE = $type:ident) => {
        #[cfg(not(target_os = "solana"))]
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
        impl $type {
            #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = toBytes))]
            pub fn to_bytes(&self) -> Box<[u8]> {
                bytes_of(self).into()
            }

            #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = fromBytes))]
            pub fn from_bytes(bytes: &[u8]) -> Result<Self, ProofDataError> {
                bytemuck::try_from_bytes(bytes)
                    .copied()
                    .map_err(|_| ProofDataError::Deserialization)
            }
        }
    };
}
pub(crate) use impl_wasm_to_bytes;
