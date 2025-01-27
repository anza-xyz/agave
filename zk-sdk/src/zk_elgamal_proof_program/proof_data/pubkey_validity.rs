//! The public-key validity proof instruction.
//!
//! A public-key validity proof system is defined with respect to an ElGamal public key. The proof
//! certifies that a given public key is a valid ElGamal public key (i.e. the prover knows a
//! corresponding secret key). To generate the proof, a prover must provide the secret key for the
//! public key.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
#[cfg(not(target_os = "solana"))]
use {
    crate::{
        encryption::elgamal::ElGamalKeypair,
        sigma_proofs::pubkey_validity::PubkeyValidityProof,
        zk_elgamal_proof_program::{
            errors::{ProofGenerationError, ProofVerificationError},
            proof_data::errors::ProofDataError,
        },
    },
    bytemuck::bytes_of,
    merlin::Transcript,
    std::convert::TryInto,
};
use {
    crate::{
        encryption::pod::elgamal::PodElGamalPubkey,
        sigma_proofs::pod::PodPubkeyValidityProof,
        zk_elgamal_proof_program::proof_data::{ProofType, ZkProofData},
    },
    bytemuck_derive::{Pod, Zeroable},
};

/// The instruction data that is needed for the `ProofInstruction::VerifyPubkeyValidity`
/// instruction.
///
/// It includes the cryptographic proof as well as the context data information needed to verify
/// the proof.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct PubkeyValidityProofData {
    /// The context data for the public key validity proof
    pub context: PubkeyValidityProofContext, // 32 bytes

    /// Proof that the public key is well-formed
    pub proof: PodPubkeyValidityProof, // 64 bytes
}

/// The context data needed to verify a pubkey validity proof.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct PubkeyValidityProofContext {
    /// The public key to be proved
    pub pubkey: PodElGamalPubkey, // 32 bytes
}

#[cfg(not(target_os = "solana"))]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl PubkeyValidityProofData {
    pub fn new(keypair: &ElGamalKeypair) -> Result<Self, ProofGenerationError> {
        let pod_pubkey = PodElGamalPubkey(keypair.pubkey().into());

        let context = PubkeyValidityProofContext { pubkey: pod_pubkey };

        let mut transcript = context.new_transcript();
        let proof = PubkeyValidityProof::new(keypair, &mut transcript).into();

        Ok(PubkeyValidityProofData { context, proof })
    }

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

impl ZkProofData<PubkeyValidityProofContext> for PubkeyValidityProofData {
    const PROOF_TYPE: ProofType = ProofType::PubkeyValidity;

    fn context_data(&self) -> &PubkeyValidityProofContext {
        &self.context
    }

    #[cfg(not(target_os = "solana"))]
    fn verify_proof(&self) -> Result<(), ProofVerificationError> {
        let mut transcript = self.context.new_transcript();
        let pubkey = self.context.pubkey.try_into()?;
        let proof: PubkeyValidityProof = self.proof.try_into()?;
        proof.verify(&pubkey, &mut transcript).map_err(|e| e.into())
    }
}

#[allow(non_snake_case)]
#[cfg(not(target_os = "solana"))]
impl PubkeyValidityProofContext {
    fn new_transcript(&self) -> Transcript {
        let mut transcript = Transcript::new(b"pubkey-validity-instruction");
        transcript.append_message(b"pubkey", bytes_of(&self.pubkey));
        transcript
    }
}

#[cfg(not(target_os = "solana"))]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl PubkeyValidityProofContext {
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_pubkey_validity_instruction_correctness() {
        let keypair = ElGamalKeypair::new_rand();

        let pubkey_validity_data = PubkeyValidityProofData::new(&keypair).unwrap();
        assert!(pubkey_validity_data.verify_proof().is_ok());
    }
}
