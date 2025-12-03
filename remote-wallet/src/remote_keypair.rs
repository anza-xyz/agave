use {
    crate::{
        ledger::get_wallet_from_info,
        locator::{Locator, Manufacturer},
        remote_wallet::{
            RemoteWallet, RemoteWalletError, RemoteWalletInfo, RemoteWalletManager,
            RemoteWalletType,
        },
        yubikey::PivSlot,
    },
    solana_derivation_path::DerivationPath,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::{Signer, SignerError},
};

pub struct RemoteKeypair {
    pub wallet_type: RemoteWalletType,
    pub derivation_path: DerivationPath,
    /// YubiKey PIV slot (analogous to derivation_path for Ledger/Trezor)
    pub yubikey_slot: Option<PivSlot>,
    pub pubkey: Pubkey,
    pub path: String,
}

impl RemoteKeypair {
    pub fn new(
        wallet_type: RemoteWalletType,
        derivation_path: DerivationPath,
        yubikey_slot: Option<PivSlot>,
        confirm_key: bool,
        path: String,
    ) -> Result<Self, RemoteWalletError> {
        let pubkey = match &wallet_type {
            RemoteWalletType::Ledger(wallet) => wallet.get_pubkey(&derivation_path, confirm_key)?,
            RemoteWalletType::Trezor(wallet) => wallet.get_pubkey(&derivation_path, confirm_key)?,
            RemoteWalletType::YubiKey(wallet) => {
                let slot = yubikey_slot.unwrap_or_default();
                wallet.get_pubkey_for_slot(slot, confirm_key)?
            }
        };

        Ok(Self {
            wallet_type,
            derivation_path,
            yubikey_slot,
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
        match &self.wallet_type {
            RemoteWalletType::Ledger(wallet) => wallet
                .sign_message(&self.derivation_path, message)
                .map_err(|e| e.into()),
            RemoteWalletType::Trezor(wallet) => wallet
                .sign_message(&self.derivation_path, message)
                .map_err(|e| e.into()),
            RemoteWalletType::YubiKey(wallet) => {
                let slot = self.yubikey_slot.unwrap_or_default();
                wallet
                    .sign_message_for_slot(slot, message)
                    .map_err(|e| e.into())
            }
        }
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
    let yubikey_slot = locator.yubikey_slot;
    let yubikey_serial = locator.yubikey_serial;
    let is_yubikey = locator.manufacturer == Manufacturer::YubiKey;

    let remote_wallet_info = RemoteWalletInfo::parse_locator(locator);
    let remote_wallet = get_wallet_from_info(remote_wallet_info, keypair_name, wallet_manager)?;

    // YubiKeys don't support derivation paths, so preserve the original slot/serial query params only
    let path = if is_yubikey {
        let mut path = "usb://yubikey".to_string();
        let mut query_parts = Vec::new();
        // Use user-specified serial, or fall back to detected device serial for reproducibility
        let serial = yubikey_serial.map(|s| s.to_string()).or_else(|| {
            let s = &remote_wallet.info.serial;
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        });
        if let Some(serial) = serial {
            query_parts.push(format!("serial={}", serial));
        }
        if let Some(slot) = yubikey_slot {
            query_parts.push(format!("slot={}", slot));
        }
        if !query_parts.is_empty() {
            path.push('?');
            path.push_str(&query_parts.join("&"));
        }
        path
    } else {
        // For Ledger/Trezor, append derivation path query
        format!("{}{}", remote_wallet.path, derivation_path.get_query())
    };

    RemoteKeypair::new(
        remote_wallet.wallet_type,
        derivation_path,
        yubikey_slot,
        confirm_key,
        path,
    )
}
