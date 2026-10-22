use {
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_system_transaction::transfer,
    solana_transaction::Transaction,
    std::{io::Error, net::SocketAddr},
};

pub fn request_airdrop_transaction(
    _faucet_addr: &SocketAddr,
    _id: &Pubkey,
    lamports: u64,
    _blockhash: Hash,
) -> Result<Transaction, Error> {
    if lamports == 0 {
        Err(Error::other("Airdrop failed"))
    } else {
        let key = Keypair::new();
        let to = solana_pubkey::new_rand();
        let blockhash = Hash::default();
        let tx = transfer(&key, &to, lamports, blockhash);
        Ok(tx)
    }
}
