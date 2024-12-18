//! `Transaction` Javascript interface
#![cfg(target_arch = "wasm32")]
#![allow(non_snake_case)]
use {
    crate::Transaction, solana_hash::Hash, solana_instruction::wasm::Instructions,
    solana_keypair::Keypair, solana_message::Message, solana_pubkey::Pubkey,
    wasm_bindgen::prelude::*,
};

#[wasm_bindgen]
impl Transaction {
    /// Create a new `Transaction`
    #[wasm_bindgen(constructor)]
    pub fn constructor(instructions: Instructions, payer: Option<Pubkey>) -> Transaction {
        let instructions: Vec<_> = instructions.into();
        Transaction::new_with_payer(&instructions, payer.as_ref())
    }

    /// Return a message containing all data that should be signed.
    #[wasm_bindgen(js_name = message)]
    pub fn js_message(&self) -> Message {
        self.message.clone()
    }

    /// Return the serialized message data to sign.
    pub fn messageData(&self) -> Box<[u8]> {
        self.message_data().into()
    }

    #[cfg(feature = "verify")]
    /// Verify the transaction
    #[wasm_bindgen(js_name = verify)]
    pub fn js_verify(&self) -> Result<(), JsValue> {
        self.verify()
            .map_err(|x| std::string::ToString::to_string(&x).into())
    }

    pub fn partialSign(&mut self, keypair: &Keypair, recent_blockhash: &Hash) {
        self.partial_sign(&[keypair], *recent_blockhash);
    }

    pub fn isSigned(&self) -> bool {
        self.is_signed()
    }

    #[cfg(feature = "bincode")]
    pub fn toBytes(&self) -> Box<[u8]> {
        bincode::serialize(self).unwrap().into()
    }

    #[cfg(feature = "bincode")]
    pub fn fromBytes(bytes: &[u8]) -> Result<Transaction, JsValue> {
        bincode::deserialize(bytes).map_err(|x| std::string::ToString::to_string(&x).into())
    }
}
