pub mod errors;
pub mod ledger;
pub mod types;

use crate::errors::RemoteWalletError;
use hidapi::{DeviceInfo, HidApi};

use types::Device;

pub trait WalletProbe {
    fn is_supported_device(&self, device_info: &hidapi::DeviceInfo) -> bool;

    fn open(&self, usb: &mut HidApi, devinfo: DeviceInfo) -> Result<Device, RemoteWalletError>;
}
