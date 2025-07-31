use crate::transport::transport_trait::Transport;
#[cfg(feature = "hidapi")]
use {
    crate::errors::RemoteWalletError,
    crate::{
        locator::{Locator, Manufacturer},
        wallet::{
            ledger::ledger::LedgerWallet,
            types::{Device, RemoteWalletType},
            WalletProbe,
        },
    },
    log::*,
    parking_lot::RwLock,
    solana_derivation_path::DerivationPath,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{
        rc::Rc,
        time::{Duration, Instant},
    },
};
#[cfg(feature = "hidapi")]
use {hidapi::DeviceInfo, parking_lot::Mutex, std::sync::Arc};

const HID_GLOBAL_USAGE_PAGE: u16 = 0xFF00;
const HID_USB_DEVICE_CLASS: u8 = 0;

// Error messages
const ERROR_HIDAPI_DISABLED: &str = "hidapi crate compilation disabled in solana-remote-wallet.";

// Logging messages
const LOG_DEVICE_SINGULAR: &str = "";
const LOG_DEVICE_PLURAL: &str = "s";

/// Manager for hardware wallet devices
///
/// This struct manages the discovery, connection, and interaction with hardware wallets.
/// It maintains a collection of connected devices and provides methods to access them.
///
/// The manager supports multiple hardware wallet types (Ledger, Keystone) and handles
/// USB HID communication through the hidapi library.
pub struct RemoteWalletManager {
    #[cfg(feature = "hidapi")]
    usb: Arc<Mutex<hidapi::HidApi>>,
    devices: RwLock<Vec<Device>>,
}

impl RemoteWalletManager {
    /// Create a new instance.
    #[cfg(feature = "hidapi")]
    pub fn new(usb: Arc<Mutex<hidapi::HidApi>>) -> Rc<Self> {
        Rc::new(Self {
            usb,
            devices: RwLock::new(Vec::new()),
        })
    }

    /// Repopulate device list by scanning for connected hardware wallets
    ///
    /// This method refreshes the USB device list and attempts to connect to all
    /// supported hardware wallets (Ledger and Keystone). It updates the internal
    /// device collection and returns the number of newly discovered devices.
    ///
    /// Returns the number of devices added (can be negative if devices were removed)
    #[cfg(feature = "hidapi")]
    pub fn update_devices(&self) -> Result<usize, RemoteWalletError> {
        let mut usb = self.usb.lock();
        usb.refresh_devices()?;

        let prev_device_count = self.devices.read().len();

        // Initialize supported wallet probes
        let probes = self.create_wallet_probes();

        // Filter HID devices and attempt to connect
        let new_devices = self.discover_and_connect_devices(&mut usb, &probes)?;

        // Update device list
        *self.devices.write() = new_devices;

        Ok(self.devices.read().len() - prev_device_count)
    }

    /// Create wallet probes for supported hardware wallets
    #[cfg(feature = "hidapi")]
    fn create_wallet_probes(&self) -> Vec<Box<dyn WalletProbe>> {
        use crate::wallet::ledger::ledger::LedgerProbe;
        vec![Box::new(LedgerProbe)]
    }

    /// Discover and connect to supported hardware wallet devices
    #[cfg(feature = "hidapi")]
    fn discover_and_connect_devices(
        &self,
        usb: &mut hidapi::HidApi,
        probes: &[Box<dyn WalletProbe>],
    ) -> Result<Vec<Device>, RemoteWalletError> {
        let valid_devices: Vec<DeviceInfo> = usb
            .device_list()
            .filter(|d| is_valid_hid_device(d.usage_page(), d.interface_number()))
            .cloned()
            .collect();

        let connection_results: Vec<Result<Device, RemoteWalletError>> = valid_devices
            .into_iter()
            .filter_map(|devinfo| {
                probes
                    .iter()
                    .find(|p| p.is_supported_device(&devinfo))
                    .map(|p| p.open(usb, devinfo))
            })
            .collect();

        // Log connection errors for debugging
        let (successful_devices, failed_connections): (Vec<_>, Vec<_>) =
            connection_results.into_iter().partition(Result::is_ok);

        if !failed_connections.is_empty() {
            debug!(
                "Failed to connect to {} device(s)",
                failed_connections.len()
            );
            for (i, err) in failed_connections.iter().enumerate() {
                debug!("Connection error {}: {:?}", i + 1, err);
            }
        }

        Ok(successful_devices
            .into_iter()
            .filter_map(Result::ok)
            .collect())
    }

    #[cfg(not(feature = "hidapi"))]
    pub fn update_devices(&self) -> Result<usize, RemoteWalletError> {
        Err(RemoteWalletError::Hid(ERROR_HIDAPI_DISABLED.to_string()))
    }

    /// List connected and acknowledged wallets
    pub fn list_devices(&self) -> Vec<RemoteWalletInfo> {
        self.devices.read().iter().map(|d| d.info.clone()).collect()
    }

    /// Get a particular wallet by host device path and extract wallet type
    fn get_wallet_by_path<T, F>(
        &self,
        host_device_path: &str,
        extractor: F,
    ) -> Result<T, RemoteWalletError>
    where
        F: FnOnce(&RemoteWalletType) -> Result<T, RemoteWalletError>,
    {
        self.devices
            .read()
            .iter()
            .find(|device| device.info.host_device_path == host_device_path)
            .ok_or(RemoteWalletError::PubkeyNotFound)
            .and_then(|device| extractor(&device.wallet_type))
    }

    /// Get a particular Ledger wallet
    #[allow(unreachable_patterns)]
    pub fn get_ledger(
        &self,
        host_device_path: &str,
    ) -> Result<Rc<LedgerWallet>, RemoteWalletError> {
        self.get_wallet_by_path(host_device_path, |wallet_type| match wallet_type {
            RemoteWalletType::Ledger(ledger) => Ok(ledger.clone()),
            _ => Err(RemoteWalletError::DeviceTypeMismatch),
        })
    }

    /// Get wallet info.
    pub fn get_wallet_info(&self, pubkey: &Pubkey) -> Option<RemoteWalletInfo> {
        self.devices
            .read()
            .iter()
            .find(|device| &device.info.pubkey == pubkey)
            .map(|device| device.info.clone())
    }

    /// Get the total number of connected devices
    pub fn device_count(&self) -> usize {
        self.devices.read().len()
    }

    /// Check if any devices are connected
    pub fn has_devices(&self) -> bool {
        !self.devices.read().is_empty()
    }

    /// Get devices by manufacturer
    pub fn get_devices_by_manufacturer(
        &self,
        manufacturer: &Manufacturer,
    ) -> Vec<RemoteWalletInfo> {
        self.devices
            .read()
            .iter()
            .filter(|device| &device.info.manufacturer == manufacturer)
            .map(|device| device.info.clone())
            .collect()
    }

    /// Get the first available wallet of any type
    ///
    /// This is a convenience method that returns the first connected wallet,
    /// regardless of its type. Useful when you just need any available wallet.
    pub fn get_first_available_wallet(&self) -> Option<RemoteWalletInfo> {
        self.devices
            .read()
            .first()
            .map(|device| device.info.clone())
    }

    /// Attempt to connect to hardware wallets with polling within a time limit
    ///
    /// This method will continuously attempt to discover and connect to hardware wallets
    /// until either devices are found or the maximum polling duration is exceeded.
    ///
    /// Returns `true` if at least one device was successfully connected, `false` otherwise
    pub fn try_connect_polling(&self, max_polling_duration: &Duration) -> bool {
        let start_time = Instant::now();
        let mut last_device_count = self.devices.read().len();

        while start_time.elapsed() <= *max_polling_duration {
            match self.update_devices() {
                Ok(new_device_count) => {
                    let current_total = self.devices.read().len();
                    if current_total > 0 {
                        let plural = if current_total == 1 {
                            LOG_DEVICE_SINGULAR
                        } else {
                            LOG_DEVICE_PLURAL
                        };
                        trace!("{} Remote Wallet{} found", current_total, plural);
                        return true;
                    }
                    last_device_count = current_total;
                }
                Err(err) => {
                    debug!("Error during device discovery: {:?}", err);
                    // Continue trying despite errors
                }
            }

            // Small delay to avoid excessive polling
            std::thread::sleep(Duration::from_millis(100));
        }

        debug!(
            "Polling timeout reached. No devices found after {:?}",
            max_polling_duration
        );
        false
    }
}

/// Trait for hardware wallet implementations
///
/// This trait defines the interface that all hardware wallet implementations must provide.
/// It includes methods for device initialization, public key derivation, and message signing.
///
/// # Type Parameters
/// * `T` - The device info type (typically `hidapi::DeviceInfo`)
#[allow(unused_variables)]
pub trait RemoteWallet<T> {
    /// Get the human-readable name of this wallet implementation
    fn name(&self) -> &str {
        "unimplemented"
    }

    /// Parse device information and initialize the wallet
    ///
    /// This method is called during device discovery to read basic information
    /// from the hardware wallet and establish communication.
    ///
    /// # Arguments
    /// * `dev_info` - Device information from the USB subsystem
    ///
    /// # Returns
    /// A `RemoteWalletInfo` structure containing device details and base public key
    fn read_device(&mut self, dev_info: &T) -> Result<RemoteWalletInfo, RemoteWalletError> {
        unimplemented!();
    }

    /// Derive a public key from the hardware wallet
    ///
    /// # Arguments
    /// * `derivation_path` - The BIP32/BIP44 derivation path
    /// * `confirm_key` - Whether to require user confirmation on the device
    ///
    /// # Returns
    /// The derived Solana public key
    fn get_pubkey(
        &self,
        derivation_path: &DerivationPath,
        confirm_key: bool,
    ) -> Result<Pubkey, RemoteWalletError> {
        unimplemented!();
    }

    /// Sign transaction data with the hardware wallet
    ///
    /// Signs raw transaction data using the private key at the specified derivation path.
    /// The path follows the Solana convention: `m/44'/501'/<account>'/<change>'`.
    ///
    /// # Arguments
    /// * `derivation_path` - The BIP32/BIP44 derivation path for the signing key
    /// * `data` - The raw transaction data to sign
    ///
    /// # Returns
    /// The signature produced by the hardware wallet
    fn sign_message(
        &self,
        derivation_path: &DerivationPath,
        data: &[u8],
    ) -> Result<Signature, RemoteWalletError> {
        unimplemented!();
    }

    /// Sign an off-chain message with the hardware wallet
    ///
    /// Signs arbitrary off-chain data using the private key at the specified derivation path.
    /// This is used for message signing that doesn't involve blockchain transactions.
    ///
    /// # Arguments
    /// * `derivation_path` - The BIP32/BIP44 derivation path for the signing key
    /// * `message` - The off-chain message data to sign
    ///
    /// # Returns
    /// The signature produced by the hardware wallet
    fn sign_offchain_message(
        &self,
        derivation_path: &DerivationPath,
        message: &[u8],
    ) -> Result<Signature, RemoteWalletError> {
        unimplemented!();
    }
}

/// Information about a connected hardware wallet device
///
/// This structure contains metadata about a hardware wallet device that has been
/// discovered and connected. It includes device identification, connection details,
/// and the device's base public key.
#[derive(Debug, Default, Clone)]
pub struct RemoteWalletInfo {
    /// Device model name (e.g., "Nano S", "Keystone Pro")
    pub model: String,
    /// Device manufacturer (Ledger, Keystone, etc.)
    pub manufacturer: Manufacturer,
    /// Device serial number for identification
    pub serial: String,
    /// Host system path to the USB device
    pub host_device_path: String,
    /// Base public key derived from the device's default derivation path
    pub pubkey: Pubkey,
    /// Error encountered during device initialization, if any
    pub error: Option<RemoteWalletError>,
}

impl RemoteWalletInfo {
    /// Create RemoteWalletInfo from a Locator
    pub fn parse_locator(locator: Locator) -> Self {
        RemoteWalletInfo {
            manufacturer: locator.manufacturer,
            pubkey: locator.pubkey.unwrap_or_default(),
            ..RemoteWalletInfo::default()
        }
    }

    /// Get a human-readable path string for this device
    ///
    /// Returns a string in the format "usb://manufacturer/pubkey" that can be
    /// used to identify this device in user interfaces or configuration files.
    pub fn get_pretty_path(&self) -> String {
        format!("usb://{}/{:?}", self.manufacturer, self.pubkey)
    }

    /// Check if this device matches another device for identification purposes
    ///
    /// Two devices are considered matching if they have the same manufacturer and
    /// either the same public key or at least one has a default (unset) public key.
    /// This allows for flexible matching during device discovery.
    pub(crate) fn matches(&self, other: &Self) -> bool {
        self.manufacturer == other.manufacturer
            && (self.pubkey == other.pubkey
                || self.pubkey == Pubkey::default()
                || other.pubkey == Pubkey::default())
    }

    /// Check if this device has an error
    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }

    /// Get the error message if any
    pub fn error_message(&self) -> Option<String> {
        self.error.as_ref().map(|e| e.to_string())
    }

    /// Check if this device is ready for use (no error and valid pubkey)
    pub fn is_ready(&self) -> bool {
        self.error.is_none() && self.pubkey != Pubkey::default()
    }
}

/// Helper to determine if a device is a valid HID device for hardware wallets
///
/// This function checks if a USB device has the correct HID characteristics
/// that are commonly used by hardware wallets. It validates either the
/// HID usage page or the USB device class.
///
/// # Arguments
/// * `usage_page` - The HID usage page identifier
/// * `interface_number` - The USB interface number/class
///
/// # Returns
/// `true` if the device appears to be a valid HID device for hardware wallets
pub fn is_valid_hid_device(usage_page: u16, interface_number: i32) -> bool {
    usage_page == HID_GLOBAL_USAGE_PAGE || interface_number == HID_USB_DEVICE_CLASS as i32
}

/// Initialize the hardware wallet manager
///
/// This function creates a new RemoteWalletManager instance with HID API support.
/// The manager can be used to discover, connect to, and interact with hardware wallets.
///
/// # Returns
/// A reference-counted RemoteWalletManager instance, or an error if initialization fails
///
/// # Errors
/// Returns an error if HID API initialization fails or if the hidapi feature is disabled
#[cfg(feature = "hidapi")]
pub fn initialize_wallet_manager() -> Result<Rc<RemoteWalletManager>, RemoteWalletError> {
    let hidapi = Arc::new(Mutex::new(hidapi::HidApi::new()?));
    Ok(RemoteWalletManager::new(hidapi))
}

/// Initialize the hardware wallet manager (hidapi disabled)
///
/// This version is compiled when the hidapi feature is disabled and always returns an error.
#[cfg(not(feature = "hidapi"))]
pub fn initialize_wallet_manager() -> Result<Rc<RemoteWalletManager>, RemoteWalletError> {
    Err(RemoteWalletError::Hid(ERROR_HIDAPI_DISABLED.to_string()))
}

/// Create a wallet manager only if hardware wallets are detected
///
/// This function initializes a wallet manager and performs an initial device scan.
/// If no devices are found, it returns `None` to avoid keeping an empty manager.
/// This is useful for applications that only need wallet functionality when hardware
/// wallets are actually connected.
///
/// Returns `Some(manager)` if devices are found, `None` if no devices are detected
pub fn maybe_wallet_manager() -> Result<Option<Rc<RemoteWalletManager>>, RemoteWalletError> {
    let wallet_manager = initialize_wallet_manager()?;
    let total_devices = wallet_manager.devices.read().len();

    // Perform initial device scan
    wallet_manager.update_devices()?;
    let found_devices = wallet_manager.devices.read().len();

    if found_devices > 0 {
        debug!(
            "Wallet manager initialized with {} device(s)",
            found_devices
        );
        Ok(Some(wallet_manager))
    } else {
        debug!("No hardware wallets detected, returning None");
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_locator() {
        let pubkey = solana_pubkey::new_rand();
        let locator = Locator {
            manufacturer: Manufacturer::Ledger,
            pubkey: Some(pubkey),
        };
        let wallet_info = RemoteWalletInfo::parse_locator(locator);
        assert!(wallet_info.matches(&RemoteWalletInfo {
            model: "nano-s".to_string(),
            manufacturer: Manufacturer::Ledger,
            serial: "".to_string(),
            host_device_path: "/host/device/path".to_string(),
            pubkey,
            error: None,
        }));

        // Test that pubkey need not be populated
        let locator = Locator {
            manufacturer: Manufacturer::Ledger,
            pubkey: None,
        };
        let wallet_info = RemoteWalletInfo::parse_locator(locator);
        assert!(wallet_info.matches(&RemoteWalletInfo {
            model: "nano-s".to_string(),
            manufacturer: Manufacturer::Ledger,
            serial: "".to_string(),
            host_device_path: "/host/device/path".to_string(),
            pubkey: Pubkey::default(),
            error: None,
        }));
    }

    #[test]
    fn test_remote_wallet_info_matches() {
        let pubkey = solana_pubkey::new_rand();
        let info = RemoteWalletInfo {
            manufacturer: Manufacturer::Ledger,
            model: "Nano S".to_string(),
            serial: "0001".to_string(),
            host_device_path: "/host/device/path".to_string(),
            pubkey,
            error: None,
        };
        let mut test_info = RemoteWalletInfo {
            manufacturer: Manufacturer::Unknown,
            ..RemoteWalletInfo::default()
        };
        assert!(!info.matches(&test_info));
        test_info.manufacturer = Manufacturer::Ledger;
        assert!(info.matches(&test_info));
        test_info.model = "Other".to_string();
        assert!(info.matches(&test_info));
        test_info.model = "Nano S".to_string();
        assert!(info.matches(&test_info));
        test_info.host_device_path = "/host/device/path".to_string();
        assert!(info.matches(&test_info));
        let another_pubkey = solana_pubkey::new_rand();
        test_info.pubkey = another_pubkey;
        assert!(!info.matches(&test_info));
        test_info.pubkey = pubkey;
        assert!(info.matches(&test_info));
    }

    #[test]
    fn test_get_pretty_path() {
        let pubkey = solana_pubkey::new_rand();
        let pubkey_str = pubkey.to_string();
        let remote_wallet_info = RemoteWalletInfo {
            model: "nano-s".to_string(),
            manufacturer: Manufacturer::Ledger,
            serial: "".to_string(),
            host_device_path: "/host/device/path".to_string(),
            pubkey,
            error: None,
        };
        assert_eq!(
            remote_wallet_info.get_pretty_path(),
            format!("usb://ledger/{pubkey_str}")
        );
    }
}