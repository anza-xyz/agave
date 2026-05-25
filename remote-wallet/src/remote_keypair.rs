use {
    crate::{
        ledger::get_wallet_from_info,
        locator::Locator,
        remote_wallet::{
            RemoteWalletError, RemoteWalletInfo, RemoteWalletManager, RemoteWalletType,
        },
    },
    solana_derivation_path::DerivationPath,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::{Signer, SignerError},
};

pub struct RemoteKeypair {
    pub wallet_type: RemoteWalletType,
    pub derivation_path: DerivationPath,
    pub pubkey: Pubkey,
    pub path: String,
}

impl RemoteKeypair {
    pub fn new(
        wallet_type: RemoteWalletType,
        derivation_path: DerivationPath,
        confirm_key: bool,
        path: String,
    ) -> Result<Self, RemoteWalletError> {
        let pubkey = wallet_type.get_pubkey(&derivation_path, confirm_key)?;

        Ok(Self {
            wallet_type,
            derivation_path,
            pubkey,
            path,
        })
    }
}

impl Signer for RemoteKeypair {
    fn try_pubkey(&self) -> Result<Pubkey, SignerError> {
        Ok(self.pubkey)
    }

    fn try_sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        self.wallet_type
            .sign_message(&self.derivation_path, message)
            .map_err(|e| e.into())
    }

    fn is_interactive(&self) -> bool {
        true
    }
}

pub fn generate_remote_keypair(
    locator: Locator,
    derivation_path: DerivationPath,
    wallet_manager: &RemoteWalletManager,
    confirm_key: bool,
    keypair_name: &str,
) -> Result<RemoteKeypair, RemoteWalletError> {
    locator.manufacturer.check_enabled()?;
    #[cfg(feature = "keystone")]
    if matches!(
        locator.manufacturer,
        crate::locator::Manufacturer::Keystone
    ) {
        return generate_keystone_remote_keypair(
            locator,
            derivation_path,
            wallet_manager,
            confirm_key,
            keypair_name,
        );
    }

    let remote_wallet_info = RemoteWalletInfo::parse_locator(locator);
    let remote_wallet = get_wallet_from_info(remote_wallet_info, keypair_name, wallet_manager)?;
    let path = format!("{}{}", remote_wallet.path, derivation_path.get_query());
    RemoteKeypair::new(
        remote_wallet.wallet_type,
        derivation_path,
        confirm_key,
        path,
    )
}

#[cfg(feature = "keystone")]
fn generate_keystone_remote_keypair(
    locator: Locator,
    derivation_path: DerivationPath,
    wallet_manager: &RemoteWalletManager,
    confirm_key: bool,
    keypair_name: &str,
) -> Result<RemoteKeypair, RemoteWalletError> {
    let manufacturer = locator.manufacturer;
    let mut remote_wallet_info = RemoteWalletInfo::parse_locator(locator);
    remote_wallet_info.pubkey = Pubkey::default();

    let remote_wallet = get_wallet_from_info(remote_wallet_info, keypair_name, wallet_manager)?;
    let wallet_type = remote_wallet.wallet_type;
    let pubkey = wallet_type.get_pubkey(&derivation_path, confirm_key)?;
    let locator = Locator::new_from_parts(manufacturer, Some(pubkey))?;
    let path = format!("{locator}{}", derivation_path.get_query());

    Ok(RemoteKeypair {
        wallet_type,
        derivation_path,
        pubkey,
        path,
    })
}
