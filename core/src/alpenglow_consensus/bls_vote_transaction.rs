use {
    super::transaction::AlpenglowVoteTransaction,
    solana_bls::{keypair::Keypair, Pubkey, PubkeyProjective, Signature, SignatureProjective},
    solana_message::VersionedMessage,
};

impl AlpenglowVoteTransaction for BlsVoteTransaction {
    fn new_for_test(bls_keypair: Keypair) -> Self {
        let message = VersionedMessage::default();
        let pubkey: Pubkey = bls_keypair.public.into();
        let signature = bls_keypair.sign(&message.serialize()).into();
        Self {
            pubkey,
            signature,
            message,
        }
    }
}

/// A vote instruction signed using BLS signatures. This format will be used
/// for vote communication between validators. This is not inteded to include
/// real Solana program instructions to be processed on-chain.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BlsVoteTransaction {
    /// Bls pubkey associated with the transaction
    pub pubkey: Pubkey,
    /// BLS signature certifying the message
    pub signature: Signature,
    /// Message signed
    pub message: VersionedMessage,
}

impl BlsVoteTransaction {
    /// Signs a versioned message
    pub fn new(message: VersionedMessage, keypair: &Keypair) -> Self {
        let message_data = message.serialize();
        let signature = keypair.sign(&message_data).into();
        let pubkey: Pubkey = keypair.public.into();
        Self {
            pubkey,
            signature,
            message,
        }
    }

    /// Verifies a signed versioned message
    pub fn verify(&self, pubkey: &Pubkey) -> bool {
        let pubkey: Result<PubkeyProjective, _> = pubkey.try_into();
        let signature: Result<SignatureProjective, _> = self.signature.try_into();
        if let (Ok(pubkey), Ok(signature)) = (pubkey, signature) {
            pubkey.verify(&signature, &self.message.serialize())
        } else {
            false
        }
    }
}
