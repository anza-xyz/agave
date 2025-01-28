use thiserror::Error;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[derive(Error, Clone, Debug, Eq, PartialEq)]
pub enum ProofDataError {
    #[error("decryption error")]
    Decryption,
    #[error("missing ciphertext")]
    MissingCiphertext,
    #[error("illegal amount bit length")]
    IllegalAmountBitLength,
    #[error("arithmetic overflow")]
    Overflow,
    #[error("invalid proof type")]
    InvalidProofType,
    #[error("deserialization failed")]
    Deserialization,
}

#[cfg(target_arch = "wasm32")]
impl From<ProofDataError> for JsValue {
    fn from(err: ProofDataError) -> Self {
        js_sys::Error::new(&err.to_string()).into()
    }
}
