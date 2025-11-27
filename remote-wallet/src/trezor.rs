use {
    crate::{
        locator::Manufacturer,
        remote_wallet::{RemoteWallet, RemoteWalletError, RemoteWalletInfo},
    },
    console::Emoji,
    solana_derivation_path::DerivationPath,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{cell::RefCell, fmt, rc::Rc},
    trezor_client::{
        client::common::handle_interaction,
        protos::{SolanaGetPublicKey, SolanaPublicKey, SolanaSignTx, SolanaTxSignature},
        Trezor,
    },
};

static CHECK_MARK: Emoji = Emoji("✅ ", "");

/// Trezor Wallet device
pub struct TrezorWallet {
    pub trezor_client: Rc<RefCell<Trezor>>,
}

impl fmt::Debug for TrezorWallet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "trezor_client")
    }
}

impl TrezorWallet {
    pub fn new(trezor_client: Trezor) -> Self {
        Self {
            trezor_client: Rc::new(RefCell::new(trezor_client)),
        }
    }
}

#[cfg(test)]
fn get_firmware_version(dev_info: &Trezor) -> Result<semver::Version, RemoteWalletError> {
    let features = dev_info
        .features()
        .ok_or(RemoteWalletError::NoDeviceFound)?;
    Ok(semver::Version::new(
        features.major_version().into(),
        features.minor_version().into(),
        features.patch_version().into(),
    ))
}

fn get_model(dev_info: &Trezor) -> Result<String, RemoteWalletError> {
    let features = dev_info
        .features()
        .ok_or(RemoteWalletError::NoDeviceFound)?;
    Ok(features.model().to_string())
}

fn get_device_id(dev_info: &Trezor) -> Result<String, RemoteWalletError> {
    let features = dev_info
        .features()
        .ok_or(RemoteWalletError::NoDeviceFound)?;
    Ok(features.device_id().to_string())
}

impl RemoteWallet<Trezor> for TrezorWallet {
    fn name(&self) -> &str {
        "Trezor hardware wallet"
    }

    /// Parse device info and get device base pubkey
    fn read_device(&mut self, dev_info: &Trezor) -> Result<RemoteWalletInfo, RemoteWalletError> {
        let model = get_model(dev_info)?;
        let serial = get_device_id(dev_info)?;
        let pubkey_result = self.get_pubkey(&DerivationPath::default(), false);
        let (pubkey, error) = match pubkey_result {
            Ok(pubkey) => (pubkey, None),
            Err(err) => (Pubkey::default(), Some(err)),
        };
        Ok(RemoteWalletInfo {
            model,
            manufacturer: Manufacturer::Trezor,
            serial,
            host_device_path: String::new(),
            pubkey,
            error,
        })
    }

    /// Get solana pubkey from a RemoteWallet
    fn get_pubkey(
        &self,
        derivation_path: &DerivationPath,
        confirm_key: bool,
    ) -> Result<Pubkey, RemoteWalletError> {
        let address_n = derivation_path.into_iter().map(|i| i.to_bits()).collect();
        let solana_get_pubkey = SolanaGetPublicKey {
            address_n,
            show_display: Some(confirm_key),
            ..SolanaGetPublicKey::default()
        };
        if confirm_key {
            println!("Waiting for your approval on {}", self.name());
        }
        let pubkey = handle_interaction(
            self.trezor_client
                .borrow_mut()
                .call(solana_get_pubkey, Box::new(|_, m: SolanaPublicKey| Ok(m)))?,
        )?;
        if confirm_key {
            println!("{CHECK_MARK}Approved");
        }
        Pubkey::try_from(pubkey.public_key())
            .map_err(|_| RemoteWalletError::Protocol("Key packet size mismatch"))
    }

    /// Sign transaction data with wallet managing pubkey at derivation path
    /// `m/44'/501'/<account>'/<change>'`.
    fn sign_message(
        &self,
        derivation_path: &DerivationPath,
        data: &[u8],
    ) -> Result<Signature, RemoteWalletError> {
        let address_n = derivation_path.into_iter().map(|i| i.to_bits()).collect();
        let solana_sign_tx = SolanaSignTx {
            address_n,
            serialized_tx: Some(data.to_vec()),
            ..SolanaSignTx::default()
        };
        let solana_tx_signature = handle_interaction(
            self.trezor_client
                .borrow_mut()
                .call(solana_sign_tx, Box::new(|_, m: SolanaTxSignature| Ok(m)))?,
        )?;
        Signature::try_from(solana_tx_signature.signature())
            .map_err(|_e| RemoteWalletError::Protocol("Signature packet size mismatch"))
    }

    /// Sign off-chain message with wallet managing pubkey at derivation path
    /// `m/44'/501'/<account>'/<change>'`.
    fn sign_offchain_message(
        &self,
        derivation_path: &DerivationPath,
        message: &[u8],
    ) -> Result<Signature, RemoteWalletError> {
        Self::sign_message(self, derivation_path, message)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        serial_test::serial,
        trezor_client::{find_devices, Model},
    };

    fn init_emulator() -> Trezor {
        let mut emulator = find_devices(false)
            .into_iter()
            .find(|t| t.model == Model::TrezorEmulator)
            .expect("An emulator should be found")
            .connect()
            .expect("Connection to the emulator should succeed");
        emulator
            .init_device(None)
            .expect("Initialization of device should succeed");
        emulator
    }

    #[test]
    #[serial]
    #[ignore]
    fn test_emulator_find() {
        let trezors = find_devices(false);
        assert!(!trezors.is_empty());
        assert!(trezors.iter().any(|t| t.model == Model::TrezorEmulator));
    }

    #[test]
    #[serial]
    #[ignore]
    fn test_solana_pubkey() {
        let mut emulator = init_emulator();
        let derivation_path_str = "m/44'/501'/0'/0'";
        let derivation_path = DerivationPath::from_absolute_path_str(derivation_path_str).unwrap();
        let address_n = derivation_path.into_iter().map(|i| i.to_bits()).collect();
        let solana_get_pubkey = SolanaGetPublicKey {
            address_n,
            show_display: Some(false),
            ..SolanaGetPublicKey::default()
        };
        let pubkey = handle_interaction(
            emulator
                .call(solana_get_pubkey, Box::new(|_, m: SolanaPublicKey| Ok(m)))
                .expect(
                    "Trezor client (the emulator) has been initialized and SolanaGetPublicKey is \
                     initialized correctly",
                ),
        )
        .expect(
            "Trezor client (the emulator) has been initialized and SolanaGetPublicKey is \
             initialized correctly",
        );
        assert!(Pubkey::try_from(pubkey.public_key()).is_ok());
    }

    #[test]
    #[serial]
    #[ignore]
    fn test_trezor_wallet() {
        let emulator = init_emulator();
        let model =
            get_model(&emulator).expect("Trezor client (the emulator) has been initialized");
        let device_id =
            get_device_id(&emulator).expect("Trezor client (the emulator) has been initialized");
        assert!(!device_id.is_empty());
        let firmware_version = get_firmware_version(&emulator);
        assert!(firmware_version.is_ok());

        let trezor_wallet = TrezorWallet::new(emulator);
        let expected_model = "T".to_string();
        assert_eq!(expected_model, model);

        let derivation_path = DerivationPath::new_bip44(Some(0), Some(0));
        let pubkey = trezor_wallet
            .get_pubkey(&derivation_path, false)
            .expect("Trezor client (the emulator) has been initialized");
        assert!(!pubkey.to_string().is_empty());
    }
}
