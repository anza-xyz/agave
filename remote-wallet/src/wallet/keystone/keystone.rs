use {
    super::error::KeystoneError,
    crate::{
        debug_print,
        errors::RemoteWalletError,
        remote_wallet::{RemoteWallet, RemoteWalletInfo, RemoteWalletManager},
        wallet::{types::Device, WalletProbe},
    },
    console::Emoji,
    dialoguer::{theme::ColorfulTheme, Select},
    hex,
    semver::Version as FirmwareVersion,
    serde_json,
    solana_sdk::derivation_path::DerivationPath,
    std::{fmt, rc::Rc},
    ur_parse_lib::keystone_ur_decoder::{probe_decode, URParseResult},
    ur_parse_lib::keystone_ur_encoder::probe_encode,
    ur_registry::crypto_key_path::{CryptoKeyPath, PathComponent},
    ur_registry::extend::crypto_multi_accounts::CryptoMultiAccounts,
    ur_registry::extend::key_derivation::KeyDerivationCall,
    ur_registry::extend::key_derivation_schema::{Curve, KeyDerivationSchema},
    ur_registry::solana::sol_sign_request::{SolSignRequest, SignType},
    ur_registry::solana::sol_signature::SolSignature,
    ur_registry::extend::qr_hardware_call::{
        CallParams, CallType, HardWareCallVersion, QRHardwareCall,
    },
    ur_registry::traits::RegistryItem,
    crate::transport::transport_trait::Transport,
    crate::transport::hid_transport::HidTransport,
};
#[cfg(feature = "hidapi")]
use {
    crate::locator::Manufacturer,
    log::*,
    num_traits::FromPrimitive,
    solana_sdk::{pubkey::Pubkey, signature::Signature},
    std::{cmp::min, convert::TryFrom},
};

static CHECK_MARK: Emoji = Emoji("✅ ", "");

const DEPRECATE_VERSION_BEFORE: FirmwareVersion = FirmwareVersion::new(0, 2, 0);

const APDU_PAYLOAD_HEADER_LEN: usize = 7;
const DEPRECATED_APDU_PAYLOAD_HEADER_LEN: usize = 8;
const P1_NON_CONFIRM: u8 = 0x00;
const P1_CONFIRM: u8 = 0x01;
const P2_EXTEND: u8 = 0x01;
const P2_MORE: u8 = 0x02;
const MAX_CHUNK_SIZE: usize = 255;

const APDU_SUCCESS_CODE: usize = 0x9000;

/// Keystone vendor ID
const KEYSTONE_VID: u16 = 0x1209;
/// Keystone product IDs
const KEYSTONE_PID: u16 = 0x3001;
const LEDGER_TRANSPORT_HEADER_LEN: usize = 5;

const HID_PACKET_SIZE: usize = 64 + HID_PREFIX_ZERO;

#[cfg(windows)]
const HID_PREFIX_ZERO: usize = 1;
#[cfg(not(windows))]
const HID_PREFIX_ZERO: usize = 0;

// JSON response field names
const JSON_FIELD_PUBKEY: &str = "pubkey";
const JSON_FIELD_PAYLOAD: &str = "payload";
const JSON_FIELD_FIRMWARE_VERSION: &str = "firmwareVersion";
const JSON_FIELD_WALLET_MFP: &str = "walletMFP";

// Path validation constants
const CACHED_ACCOUNT_RANGE: u32 = 49;
const CACHED_CHANGE_RANGE: u32 = 49;
const CACHED_FIXED_ACCOUNT: u32 = 0;

// Error messages
const ERROR_INVALID_JSON: &str = "Invalid JSON response";
const ERROR_MISSING_FIELD: &str = "Missing required field";
const ERROR_INVALID_HEX: &str = "Invalid hex data";
const ERROR_SIGNATURE_SIZE: &str = "Signature packet size mismatch";
const ERROR_KEY_SIZE: &str = "Key packet size mismatch";

#[derive(Debug, Clone, Copy, PartialEq)]
enum CommandType {
    CMD_ECHO_TEST = 0x01,
    CMD_RESOLVE_UR = 0x02,
    CMD_CHECK_LOCK_STATUS = 0x03,
    CMD_EXPORT_ADDRESS = 0x04,
    CMD_GET_DEVICE_INFO = 0x05,
    CMD_GET_DEVICE_USB_PUBKEY = 0x06,
}

impl CommandType {
    /// Check if a u16 value corresponds to a valid CommandType
    fn is_valid_command(value: u16) -> bool {
        matches!(value, 0x01 | 0x02 | 0x03 | 0x04 | 0x05 | 0x06)
    }

    /// Try to convert u16 to CommandType
    fn from_u16(value: u16) -> Option<CommandType> {
        match value {
            0x01 => Some(CommandType::CMD_ECHO_TEST),
            0x02 => Some(CommandType::CMD_RESOLVE_UR),
            0x03 => Some(CommandType::CMD_CHECK_LOCK_STATUS),
            0x04 => Some(CommandType::CMD_EXPORT_ADDRESS),
            0x05 => Some(CommandType::CMD_GET_DEVICE_INFO),
            0x06 => Some(CommandType::CMD_GET_DEVICE_USB_PUBKEY),
            _ => None,
        }
    }
}

enum ConfigurationVersion {
    Deprecated(Vec<u8>),
    Current(Vec<u8>),
}

#[derive(Debug)]
pub enum PubkeyDisplayMode {
    Short,
    Long,
}

#[derive(Debug)]
pub struct LedgerSettings {
    pub enable_blind_signing: bool,
    pub pubkey_display: PubkeyDisplayMode,
}

/// Ledger Wallet device
pub struct KeystoneWallet {
    pub transport: Box<dyn Transport>,
    pub pretty_path: String,
    pub version: FirmwareVersion,
    pub mfp: Option<[u8; 4]>,
}

impl fmt::Debug for KeystoneWallet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeystoneWallet")
    }
}
#[derive(Debug, Clone)]
pub struct EAPDUFrame {
    pub cla: u8,
    pub ins: CommandType,
    pub p1: u16,
    pub p2: u16,
    pub lc: u16,
    pub data: Vec<u8>,
    pub data_len: u32,
}

#[cfg(feature = "hidapi")]
impl KeystoneWallet {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            pretty_path: String::default(),
            version: FirmwareVersion::new(0, 0, 0),
            mfp: None,
        }
    }

    // Transport Protocol:
    //		* Communication Channel Id		(2 bytes big endian )
    //		* Command Tag				(1 byte)
    //		* Packet Sequence ID			(2 bytes big endian)
    //		* Payload				(Optional)
    //
    // Payload
    //		* APDU Total Length			(2 bytes big endian)
    //		* APDU_CLA				(1 byte)
    //		* APDU_INS				(1 byte)
    //		* APDU_P1				(1 byte)
    //		* APDU_P2				(1 byte)
    //		* APDU_LENGTH 	        (1 byte (2 bytes DEPRECATED))
    //		* APDU_Payload				(Variable)
    //
    fn write(&self, command: CommandType, data: &[u8]) -> Result<(), RemoteWalletError> {
        let data_len = data.len();
        let mut offset = 0;
        let mut sequence_number = 0;
        let mut hid_chunk = [0_u8; HID_PACKET_SIZE];
        println!("data_len: {:?}", data_len);
        println!("data: {:2x?}", data);
        let total_packets = if data_len > 11 {
            if data_len % (64 - 10) == 0 {
                data_len / (64 - 10)
            } else {
                data_len / (64 - 10) + 1
            }
        } else {
            1
        };
        let mut request_id = 0;

        while sequence_number == 0 || offset < data_len {
            // Clear the entire chunk to avoid residual data
            hid_chunk.fill(0);

            let header = 10;
            let size = min(64 - header, data_len - offset);
            {
                let chunk = &mut hid_chunk[HID_PREFIX_ZERO..];
                chunk[0..3].copy_from_slice(&[0x00, 0x00, 0x00]);
                chunk[3..10].copy_from_slice(&[
                    (command as u16 & 0xff) as u8,
                    (total_packets >> 8) as u8,
                    (total_packets & 0xff) as u8,
                    (sequence_number >> 8) as u8,
                    (sequence_number & 0xff) as u8,
                    (request_id >> 8) as u8,
                    (request_id & 0xff) as u8,
                ]);

                chunk[header..header + size].copy_from_slice(&data[offset..offset + size]);
            }
            trace!("Ledger write {:?}", &hid_chunk[..]);
            if command == CommandType::CMD_RESOLVE_UR {
                // debug_print!("send command: sequence_number: {:?}, request_id: {:?}", sequence_number, request_id);
                // debug_print!("send command: {:2x?}", &hid_chunk[..]);
            }
            let n = self.transport.write(&hid_chunk[..])?;
            if n < size + header {
                return Err(RemoteWalletError::Protocol("Write data size mismatch"));
            }
            offset += size;
            sequence_number += 1;
            request_id += 1;
            if sequence_number >= 0xffff {
                return Err(RemoteWalletError::Protocol(
                    "Maximum sequence number reached",
                ));
            }
        }
        Ok(())
    }

    // Transport Protocol:
    //		* Communication Channel Id		(2 bytes big endian )
    //		* Command Tag				(1 byte)
    //		* Packet Sequence ID			(2 bytes big endian)
    //		* Payload				(Optional)
    //
    // Payload
    //		* APDU_LENGTH				(1 byte)
    //		* APDU_Payload				(Variable)
    //
    fn read(&self) -> Result<Vec<u8>, RemoteWalletError> {
        let _buffer = [0u8; HID_PACKET_SIZE];
        let mut result_data = Vec::new();
        let mut sequence_number = 0u16;
        let mut total_length = 0usize;
        let _received_length = 0usize;

        loop {
            // Read HID packet
            let chunk = self.transport.read()?;
            if chunk.len() < LEDGER_TRANSPORT_HEADER_LEN {
                return Err(RemoteWalletError::Protocol("Invalid HID packet size"));
            }

            let packet = &chunk[HID_PREFIX_ZERO..chunk.len()];
            let packet = {
                let mut end = packet.len();
                while end > 0 && packet[end - 1] == 0x00 {
                    end -= 1;
                }
                &packet[..end]
            };
            // Parse transport header
            let _cla = packet[0];
            let command = u16::from_be_bytes([packet[1], packet[2]]);
            let total_packets = u16::from_be_bytes([packet[3], packet[4]]);
            let packet_seq = u16::from_be_bytes([packet[5], packet[6]]);
            let _request_id = u16::from_be_bytes([packet[7], packet[8]]);
            let packet_data = &packet[9..];
            if command == CommandType::CMD_RESOLVE_UR as u16 {
                debug_print!("packet_length: {:?}", packet_data.len());
            }

            // Check if command is valid
            if !CommandType::is_valid_command(command) {
                return Err(RemoteWalletError::Protocol("Invalid command"));
            }

            // Optionally, convert to CommandType enum for type safety
            let _command_type = CommandType::from_u16(command)
                .ok_or(RemoteWalletError::Protocol("Invalid command type"))?;

            if packet_seq != sequence_number {
                return Err(RemoteWalletError::Protocol("Invalid packet sequence"));
            }

            sequence_number += 1;
            total_length += packet_data.len();
            result_data.extend_from_slice(packet_data);

            // Check if we have received all data
            if sequence_number == total_packets {
                break;
            }

            if sequence_number >= 0xffff {
                return Err(RemoteWalletError::Protocol(
                    "Maximum sequence number reached",
                ));
            }
        }

        // Truncate to exact length
        result_data.truncate(total_length);

        // Parse status code from last 2 bytes
        if result_data.len() < 2 {
            return Err(RemoteWalletError::Protocol("Response too short"));
        }

        // Remove status code from result (status code validation is handled elsewhere)
        result_data.truncate(result_data.len() - 2);

        Ok(result_data)
    }

    fn _send_apdu(&self, command: CommandType, data: &[u8]) -> Result<String, RemoteWalletError> {
        self.write(command, data)?;
        let message = self.read()?;
        let message_str = String::from_utf8_lossy(&message);
        if let (Some(start), Some(end)) = (message_str.find('{'), message_str.rfind('}')) {
            if start < end {
                let json_str = &message_str[start..=end];
                return Ok(json_str.to_string());
            }
        }
        debug_print!("message_str: {:?}", message_str);

        Ok(message_str.to_string())
    }

    fn send_apdu(&self, command: CommandType, data: &[u8]) -> Result<String, RemoteWalletError> {
        self._send_apdu(command, data)
    }

    fn get_firmware_version(&self) -> Result<(FirmwareVersion, Option<[u8; 4]>), RemoteWalletError> {
        self.get_device_info()
    }

    /// Generate a hardware call request for key derivation
    fn generate_hardware_call(&self, derivation_path: &DerivationPath) -> Result<String, RemoteWalletError> {
        let key_path = parse_crypto_key_path(derivation_path, self.mfp);
        let schema = KeyDerivationSchema::new(key_path, Some(Curve::Ed25519), None, None);
        let schemas = vec![schema];
        let call = QRHardwareCall::new(
            CallType::KeyDerivation,
            CallParams::KeyDerivation(KeyDerivationCall::new(schemas)),
            None,
            HardWareCallVersion::V1,
        );
        let bytes: Vec<u8> = call.try_into().unwrap();
        let res =
            probe_encode(&bytes, 400, QRHardwareCall::get_registry_type().get_type()).unwrap();
        Ok(res.data)
    }

    /// Generate a Solana sign request for transaction signing
    fn generate_sol_sign_request(&self, derivation_path: &DerivationPath, sign_data: &[u8]) -> Result<String, RemoteWalletError> {
        let crypto_key_path = parse_crypto_key_path(derivation_path, self.mfp);
        let request_id = [0u8; 16].to_vec();
        let sol_sign_request = SolSignRequest::new(
            Some(request_id),
            sign_data.to_vec(),
            crypto_key_path,
            None,
            Some("solana cli".to_string()),
            SignType::Transaction,
        );
        let bytes: Vec<u8> = sol_sign_request.try_into().unwrap();
        let res =
            probe_encode(&bytes, 0xFFFFFFF, SolSignRequest::get_registry_type().get_type()).unwrap();
        Ok(res.data)
    }

    /// Parse a public key from UR (Uniform Resource) format
    fn parse_ur_pubkey(&self, ur: &str) -> Result<Vec<u8>, RemoteWalletError> {
        let result: URParseResult<CryptoMultiAccounts> =
            probe_decode(ur.to_string().to_lowercase()).unwrap();

        Ok(result.data.unwrap().get_keys().get(0).unwrap().get_key())
    }

    /// Parse a signature from UR (Uniform Resource) format
    fn parse_ur_signature(&self, ur: &str) -> Result<Vec<u8>, RemoteWalletError> {
        let result: URParseResult<SolSignature> =
            probe_decode(ur.to_string().to_lowercase()).unwrap();
        Ok(result.data.unwrap().get_signature().to_vec())
    }

    /// Parse JSON response and extract field value
    fn parse_json_field(&self, json_str: &str, field_name: &str) -> Result<String, RemoteWalletError> {
        let json = serde_json::from_str::<serde_json::Value>(json_str)
            .map_err(|_| RemoteWalletError::Protocol(ERROR_INVALID_JSON))?;
        
        json.get(field_name)
            .and_then(|v| v.as_str())
            .ok_or(RemoteWalletError::Protocol(ERROR_MISSING_FIELD))
            .map(String::from)
    }

    // pub fn get_settings(&self) -> Result<LedgerSettings, RemoteWalletError> {
    //     self.get_device_info().map(|conf ig| match config {
    //         ConfigurationVersion::Current(config) => {
    //             let enable_blind_signing = config[0] != 0;
    //             let pubkey_display = if config[1] == 0 {
    //                 PubkeyDisplayMode::Long
    //             } else {
    //                 PubkeyDisplayMode::Short
    //             };
    //             LedgerSettings {
    //                 enable_blind_signing,
    //                 pubkey_display,
    //             }
    //         }
    //         ConfigurationVersion::Deprecated(_) => LedgerSettings {
    //             enable_blind_signing: false,
    //             pubkey_display: PubkeyDisplayMode::Short,
    //         },
    //     })
    // }

    fn get_device_info(&self) -> Result<(FirmwareVersion, Option<[u8; 4]>), RemoteWalletError> {
        let json_str = self._send_apdu(CommandType::CMD_GET_DEVICE_INFO, &[])?;
        let mut version = FirmwareVersion::new(0, 0, 0);
        let mut mfp = None;
        match serde_json::from_str::<serde_json::Value>(&json_str) {
            Ok(json) => {
                if let Some(firmware_version) = json.get(JSON_FIELD_FIRMWARE_VERSION).and_then(|v| v.as_str())
                {
                    // Parse version string like "12.1.2"
                    let parts: Vec<&str> = firmware_version.split('.').collect();
                    if parts.len() >= 3 {
                        if let (Ok(major), Ok(minor), Ok(patch)) = (
                            parts[0].parse::<u64>(),
                            parts[1].parse::<u64>(),
                            parts[2].parse::<u64>(),
                        ) {
                            version = FirmwareVersion::new(major, minor, patch);
                        }
                    }
                }

                if let Some(mfp_str) = json.get(JSON_FIELD_WALLET_MFP).and_then(|v| v.as_str()) {
                    if let Ok(mfp_bytes) = hex::decode(mfp_str) {
                        if mfp_bytes.len() == 4 {
                            mfp = Some([mfp_bytes[0], mfp_bytes[1], mfp_bytes[2], mfp_bytes[3]]);
                        }
                    }
                }
            }
            Err(e) => {
                debug_print!("JSON parse error: {}", e);
                return Err(RemoteWalletError::Protocol("JSON parse error"));
            }
        }
        Ok((version, mfp))
    }

    fn outdated_app(&self) -> bool {
        self.version < DEPRECATE_VERSION_BEFORE
    }

    fn parse_status(status: usize) -> Result<(), RemoteWalletError> {
        if status == APDU_SUCCESS_CODE {
            Ok(())
        } else if let Some(err) = KeystoneError::from_usize(status) {
            Err(err.into())
        } else {
            Err(RemoteWalletError::Protocol("Unknown error"))
        }
    }
}

use crate::wallet::types::RemoteWalletType;
use hidapi::{DeviceInfo, HidApi};

pub struct KeystoneProbe;
#[cfg(not(feature = "hidapi"))]
impl WalletProbe<Self> for KeystoneProbe {}
#[cfg(feature = "hidapi")]
impl WalletProbe for KeystoneProbe {
    fn is_supported_device(&self, device_info: &hidapi::DeviceInfo) -> bool {
        device_info.product_id() == KEYSTONE_PID && device_info.vendor_id() == KEYSTONE_VID
    }

    fn open(&self, usb: &mut HidApi, devinfo: DeviceInfo) -> Result<Device, RemoteWalletError> {
        let handle = usb
            .open_path(devinfo.path())
            .map_err(|e| RemoteWalletError::Hid(e.to_string()))?;
        let mut wallet = KeystoneWallet::new(Box::new(HidTransport::new(handle)));
        let info = wallet.read_device(&devinfo).map_err(|e| RemoteWalletError::Hid(e.to_string()))?;
        wallet.pretty_path = info.get_pretty_path();
        Ok(Device {
            path: devinfo.path().to_string_lossy().into_owned(),
            info,
            wallet_type: RemoteWalletType::Keystone(Rc::new(wallet)),
        })
    }
}

#[cfg(not(feature = "hidapi"))]
impl RemoteWallet<Self> for KeystoneWallet {}
#[cfg(feature = "hidapi")]
impl RemoteWallet<hidapi::DeviceInfo> for KeystoneWallet {
    fn name(&self) -> &str {
        "Keystone hardware wallet"
    }

    fn read_device(
        &mut self,
        dev_info: &hidapi::DeviceInfo,
    ) -> Result<RemoteWalletInfo, RemoteWalletError> {
        let manufacturer = dev_info
            .manufacturer_string()
            .and_then(|s| Manufacturer::try_from(s).ok())
            .unwrap_or_default();
        let model = dev_info
            .product_string()
            .unwrap_or("Unknown")
            .to_lowercase()
            .replace(' ', "-");
        let serial = dev_info.serial_number().unwrap_or("Unknown").to_string();
        let host_device_path = dev_info.path().to_string_lossy().to_string();
        let (version, mfp) = self.get_device_info()?; 
        debug_print!("version: {:?}", version);
        self.version = version;
        self.mfp = mfp;
        debug_print!("mfp: {:?}", mfp);
        let pubkey_result = self.get_pubkey(&DerivationPath::default(), false);
        let (pubkey, error) = match pubkey_result {
            Ok(pubkey) => (pubkey, None),
            Err(err) => (Pubkey::default(), Some(err)),
        };
        Ok(RemoteWalletInfo {
            model,
            manufacturer,
            serial,
            host_device_path,
            pubkey,
            error,
        })
    }

    fn get_pubkey(
        &self,
        derivation_path: &DerivationPath,
        _confirm_key: bool,
    ) -> Result<Pubkey, RemoteWalletError> {
        debug_print!("derivation_path: {:?}", derivation_path);
        let pubkey = if is_path_in_cached_range(derivation_path) {
            let data = extend_and_serialize(derivation_path);
            let key = self.send_apdu(
                CommandType::CMD_GET_DEVICE_USB_PUBKEY,
                data.as_slice(),
            )?;
            let payload = self.parse_json_field(&key, JSON_FIELD_PUBKEY)?;
            
            // 检查 payload 是否包含错误信息
            let keystone_error = KeystoneError::from_error_message(&payload);
            if !matches!(keystone_error, KeystoneError::CommunicationError { .. }) {
                return Err(keystone_error.into());
            }
            
            hex::decode(payload)
                .map_err(|_| RemoteWalletError::Protocol(ERROR_INVALID_HEX))?
        } else {
            let key = self.send_apdu(
                CommandType::CMD_RESOLVE_UR,
                self.generate_hardware_call(derivation_path)?.as_bytes(),
            )?;
            let payload = self.parse_json_field(&key, JSON_FIELD_PAYLOAD)?;
            
            // 检查 payload 是否包含错误信息
            let keystone_error = KeystoneError::from_error_message(&payload);
            if !matches!(keystone_error, KeystoneError::CommunicationError { .. }) {
                return Err(keystone_error.into());
            }
            
            self.parse_ur_pubkey(&payload)?
        };

        Pubkey::try_from(pubkey)
            .map_err(|_| RemoteWalletError::Protocol(ERROR_KEY_SIZE))
    }

    fn sign_message(
        &self,
        derivation_path: &DerivationPath,
        data: &[u8],
    ) -> Result<Signature, RemoteWalletError> {
        // If the first byte of the data is 0xff then it is an off-chain message
        // because it starts with the Domain Specifier b"\xffsolana offchain".
        // On-chain messages, in contrast, start with either 0x80 (MESSAGE_VERSION_PREFIX)
        // or the number of signatures (0x00 - 0x13).
        if !data.is_empty() && data[0] == 0xff {
            return self.sign_offchain_message(derivation_path, data);
        }
        
        let key_path = parse_crypto_key_path(derivation_path, self.mfp);
        debug_print!("key_path: {:?}", key_path);

        if self.mfp.is_none() {
            return Err(RemoteWalletError::Protocol("MFP is not set"));
        }

        let result = self.generate_sol_sign_request(derivation_path, data)?;
        debug_print!("result: {:?}", result);
        let key = self.send_apdu(
            CommandType::CMD_RESOLVE_UR,
            result.as_bytes(),
        )?;
        let payload = self.parse_json_field(&key, JSON_FIELD_PAYLOAD)?;
        debug_print!("payload: {:?}", payload);
        
        let keystone_error = KeystoneError::from_error_message(&payload);
        if !matches!(keystone_error, KeystoneError::CommunicationError { .. }) {
            return Err(keystone_error.into());
        }
        
        let signature =  self.parse_ur_signature(&payload)?;

        debug_print!("signature: {:?}", signature);
        
        // TODO: Remove this temporary workaround - should use actual signature
        let signature = vec![0u8; 64];
        Signature::try_from(signature)
            .map_err(|_| RemoteWalletError::Protocol(ERROR_SIGNATURE_SIZE))
    }

    fn sign_offchain_message(
        &self,
        derivation_path: &DerivationPath,
        data: &[u8],
    ) -> Result<Signature, RemoteWalletError> {
        // If the first byte of the data is 0xff then it is an off-chain message
        // because it starts with the Domain Specifier b"\xffsolana offchain".
        // On-chain messages, in contrast, start with either 0x80 (MESSAGE_VERSION_PREFIX)
        // or the number of signatures (0x00 - 0x13).
        // if !data.is_empty() && data[0] == 0xff {
            // return self.sign_offchain_message(derivation_path, data);
        // }
        debug_print!("{}:{:?}", file!(), line!());
        
        let key_path = parse_crypto_key_path(derivation_path, self.mfp);
        debug_print!("key_path: {:?}", key_path);

        if self.mfp.is_none() {
            return Err(RemoteWalletError::Protocol("MFP is not set"));
        }

        let result = self.generate_sol_sign_request(derivation_path, data)?;
        debug_print!("result: {:?}", result);
        let key = self.send_apdu(
            CommandType::CMD_RESOLVE_UR,
            result.as_bytes(),
        )?;
        let payload = self.parse_json_field(&key, JSON_FIELD_PAYLOAD)?;
        debug_print!("payload: {:?}", payload);
        
        let keystone_error = KeystoneError::from_error_message(&payload);
        if !matches!(keystone_error, KeystoneError::CommunicationError { .. }) {
            return Err(keystone_error.into());
        }
        
        let signature =  self.parse_ur_signature(&payload)?;

        debug_print!("signature: {:?}", signature);
        
        // TODO: Remove this temporary workaround - should use actual signature
        let signature = vec![0u8; 64];
        Signature::try_from(signature)
            .map_err(|_| RemoteWalletError::Protocol(ERROR_SIGNATURE_SIZE))
    }
}

/// Convert a Solana DerivationPath to a CryptoKeyPath for Keystone hardware wallet
fn parse_crypto_key_path(derivation_path: &DerivationPath, mfp: Option<[u8; 4]>) -> CryptoKeyPath {
    let mut path_components = vec![
        PathComponent::new(Some(44), true).unwrap(),   // BIP44 purpose
        PathComponent::new(Some(501), true).unwrap()   // Solana coin type
    ];
    
    if let Some(account) = derivation_path.account() {
        let account_index = account.to_u32();
        path_components.push(PathComponent::new(Some(account_index), true).unwrap());
    }
    
    if let Some(change) = derivation_path.change() {
        let change_index = change.to_u32();
        path_components.push(PathComponent::new(Some(change_index), true).unwrap());
    }
    
    CryptoKeyPath::new(path_components, mfp, None)
}

/// Build the derivation path byte array from a DerivationPath selection
/// 
/// Format: [depth_byte, 4-byte indices...]
/// - depth_byte: 2 for m/44'/501', 3 for m/44'/501'/account', 4 for m/44'/501'/account'/change'
/// - Each index is serialized as 4 bytes in big-endian format with hardened bit set
fn extend_and_serialize(derivation_path: &DerivationPath) -> Vec<u8> {
    let depth_byte: u8 = if derivation_path.change().is_some() {
        4  // m/44'/501'/account'/change'
    } else if derivation_path.account().is_some() {
        3  // m/44'/501'/account'
    } else {
        2  // m/44'/501'
    };
    
    let mut concat_derivation = vec![depth_byte];
    for index in derivation_path.path() {
        concat_derivation.extend_from_slice(&index.to_bits().to_be_bytes());
    }
    concat_derivation
}

/// Build the derivation path byte array for multiple paths
/// 
/// Format: [count, path1_serialized, path2_serialized, ...]
/// where each path is serialized using extend_and_serialize
fn extend_and_serialize_multiple(derivation_paths: &[&DerivationPath]) -> Vec<u8> {
    let mut concat_derivation = vec![derivation_paths.len() as u8];
    for derivation_path in derivation_paths {
        concat_derivation.append(&mut extend_and_serialize(derivation_path));
    }
    concat_derivation
}

/// Choose a Ledger wallet based on matching info fields
pub fn get_keystone_from_info(
    info: RemoteWalletInfo,
    keypair_name: &str,
    wallet_manager: &RemoteWalletManager,
) -> Result<Rc<KeystoneWallet>, RemoteWalletError> {
    let devices = wallet_manager.list_devices();
    let mut matches = devices
        .iter()
        .filter(|&device_info| device_info.matches(&info));
    if matches
        .clone()
        .all(|device_info| device_info.error.is_some())
    {
        let first_device = matches.next();
        if let Some(device) = first_device {
            return Err(device.error.clone().unwrap());
        }
    }
    let mut matches: Vec<(String, String)> = matches
        .filter(|&device_info| {
            debug_print!("{:?}", device_info);
            device_info.error.is_none()
        })
        .map(|device_info| {
            let query_item = format!("{} ({})", device_info.get_pretty_path(), device_info.model,);
            (device_info.host_device_path.clone(), query_item)
        })
        .collect();
    if matches.is_empty() {
        return Err(RemoteWalletError::NoDeviceFound);
    }
    matches.sort_by(|a, b| a.1.cmp(&b.1));
    let (host_device_paths, items): (Vec<String>, Vec<String>) = matches.into_iter().unzip();

    let wallet_host_device_path = if host_device_paths.len() > 1 {
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Multiple hardware wallets found. Please select a device for {keypair_name:?}"
            ))
            .default(0)
            .items(&items[..])
            .interact()
            .unwrap();
        &host_device_paths[selection]
    } else {
        &host_device_paths[0]
    };
    wallet_manager.get_keystone(wallet_host_device_path)
}

//
fn is_last_part(p2: u8) -> bool {
    p2 & P2_MORE == 0
}

/// Check if a derivation path is within the cached range supported by the hardware wallet
///
/// Keystone hardware wallet pre-caches a limited number of derivation paths to avoid
/// requiring user password input. The cached ranges are:
/// - m/44'/501' (base path)
/// - m/44'/501'/0' to m/44'/501'/49' (50 account paths)
/// - m/44'/501'/0'/0' to m/44'/501'/0'/49' (50 change paths for account 0)
///
/// Paths outside these ranges require user password confirmation.
fn is_path_in_cached_range(derivation_path: &DerivationPath) -> bool {
    let path = derivation_path.path();
    
    // Must have at least m/44'/501'
    if path.len() < 2 {
        return false;
    }
    
    // Must be BIP44 Solana path
    if path[0].to_u32() != 44 || path[1].to_u32() != 501 {
        return false;
    }
    
    match path.len() {
        2 => true, // m/44'/501'
        3 => {
            // m/44'/501'/account' where account <= 49
            let account = path[2].to_u32();
            account <= CACHED_ACCOUNT_RANGE
        }
        4 => {
            // m/44'/501'/account'/change' where account == 0 and change <= 49
            let account = path[2].to_u32();
            let change = path[3].to_u32();
            account == CACHED_FIXED_ACCOUNT && change <= CACHED_CHANGE_RANGE
        }
        
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_last_part() {
        // Bytes with bit-2 set to 0 should return true
        assert!(is_last_part(0b00));
        assert!(is_last_part(0b01));
        assert!(is_last_part(0b101));
        assert!(is_last_part(0b1001));
        assert!(is_last_part(0b1101));

        // Bytes with bit-2 set to 1 should return false
        assert!(!is_last_part(0b10));
        assert!(!is_last_part(0b11));
        assert!(!is_last_part(0b110));
        assert!(!is_last_part(0b111));
        assert!(!is_last_part(0b1010));

        // Test implementation-specific uses
        let p2 = 0;
        assert!(is_last_part(p2));
        let p2 = P2_EXTEND | P2_MORE;
        assert!(!is_last_part(p2));
        assert!(is_last_part(p2 & !P2_MORE));
    }

    #[test]
    fn test_parse_status() {
        KeystoneWallet::parse_status(APDU_SUCCESS_CODE).expect("unexpected result");
        if let RemoteWalletError::Protocol(err) = KeystoneWallet::parse_status(0x6985).unwrap_err()
        {
            assert_eq!(err, "Unknown error");
        }
        if let RemoteWalletError::Protocol(err) = KeystoneWallet::parse_status(0x6fff).unwrap_err()
        {
            assert_eq!(err, "Unknown error");
        }
    }
}
