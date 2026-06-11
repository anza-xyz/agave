use {
    crate::{
        locator::Manufacturer,
        remote_wallet::{RemoteWallet, RemoteWalletError, RemoteWalletInfo},
    },
    console::Emoji,
    hex,
    semver::Version as FirmwareVersion,
    serde_json,
    solana_derivation_path::DerivationPath,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{convert::TryFrom, fmt, time::Duration},
    ur_parse_lib::{keystone_ur_decoder::probe_decode, keystone_ur_encoder::probe_encode},
    ur_registry::{
        crypto_key_path::{CryptoKeyPath, PathComponent},
        extend::{
            crypto_multi_accounts::CryptoMultiAccounts,
            key_derivation::KeyDerivationCall,
            key_derivation_schema::{Curve, KeyDerivationSchema},
            qr_hardware_call::{CallParams, CallType, HardWareCallVersion, QRHardwareCall},
        },
        solana::{
            sol_sign_request::{SignType, SolSignRequest},
            sol_signature::SolSignature,
        },
        traits::RegistryItem,
    },
};

static CHECK_MARK: Emoji = Emoji("✅ ", "");

const REQUEST_ID: u16 = 0x0000;

/// Keystone vendor ID
const KEYSTONE_VID: u16 = 0x1209;
/// Keystone product ID
const KEYSTONE_PID: u16 = 0x3001;

const HID_PACKET_SIZE: usize = 64;
const EAPDU_OFFSET_CLA: usize = 0;
const EAPDU_OFFSET_INS: usize = 1;
const EAPDU_OFFSET_P1: usize = 3;
const EAPDU_OFFSET_P2: usize = 5;
const EAPDU_OFFSET_LC: usize = 7;
const EAPDU_OFFSET_CDATA: usize = 9;
const EAPDU_RESPONSE_STATUS_LEN: usize = 2;
const EAPDU_MAX_REQ_DATA_PER_PACKET: usize = HID_PACKET_SIZE - EAPDU_OFFSET_CDATA;
const EAPDU_SUCCESS_STATUS: u16 = 0x0000;
const EAPDU_EXPORT_ADDRESS_PAGE_STATUS: u16 = 0x0006;
// USB operations are user-interactive and may wait on device screen approval.
// Keystone signing requires user approval on the device, so allow enough time
// for users to review and confirm requests before the USB read/write times out.
const USB_TIMEOUT: Duration = Duration::from_secs(60);
// Use a short timeout so stale packets are drained without delaying new requests.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(20);
// Bound stale packet draining to at most one full-sized response window.
const MAX_DRAIN_PACKETS: usize = 64;
// Maximum UR fragment size; large enough to keep USB requests single-part.
const MAX_UR_FRAGMENT_LEN: usize = 0x0FFF_FFFF;

// JSON response field names
const JSON_FIELD_PUBKEY: &str = "pubkey";
const JSON_FIELD_PAYLOAD: &str = "payload";
const JSON_FIELD_ERROR: &str = "error";
const JSON_FIELD_FIRMWARE_VERSION: &str = "firmwareVersion";
const JSON_FIELD_WALLET_MFP: &str = "walletMFP";

// Error messages
const ERROR_INVALID_JSON: &str = "Invalid JSON response";
const ERROR_MISSING_FIELD: &str = "Missing required field";
const ERROR_INVALID_HEX: &str = "Invalid hex data";
const ERROR_SIGNATURE_SIZE: &str = "Signature packet size mismatch";
const ERROR_KEY_SIZE: &str = "Key packet size mismatch";
const ERROR_EXPORT_ADDRESS_PAGE: &str = "Export address is only allowed on specific pages";

// Keystone cached derivation-path ranges for USB pubkey export.
const CACHED_ACCOUNT_RANGE: u32 = 49;
const CACHED_FIXED_CHANGE: u32 = 0;
const SOLANA_COIN_TYPE: u32 = 501;

#[derive(Debug, Clone, Copy, PartialEq)]
enum CommandType {
    CmdEchoTest = 0x01,
    CmdResolveUR = 0x02,
    CmdCheckLockStatus = 0x03,
    CmdExportAddress = 0x04,
    CmdGetDeviceInfo = 0x05,
    CmdGetDeviceUSBPubkey = 0x06,
}

#[derive(Clone, Copy)]
struct UsbIo {
    interface_number: u8,
    setting_number: u8,
    endpoint_out: u8,
    endpoint_in: u8,
    transfer_type: rusb::TransferType,
}

struct EndpointPair {
    output: u8,
    input: u8,
}

struct EapduHeader {
    command: u16,
    total_packets: u16,
    packet_sequence: u16,
    request_id: u16,
}

impl EapduHeader {
    fn parse(packet: &[u8]) -> Result<Self, RemoteWalletError> {
        if packet.len() < EAPDU_OFFSET_CDATA + EAPDU_RESPONSE_STATUS_LEN {
            return Err(RemoteWalletError::Protocol("Invalid EAPDU packet size"));
        }

        let command = u16::from_be_bytes([packet[EAPDU_OFFSET_INS], packet[EAPDU_OFFSET_INS + 1]]);
        let total_packets =
            u16::from_be_bytes([packet[EAPDU_OFFSET_P1], packet[EAPDU_OFFSET_P1 + 1]]);
        let packet_sequence =
            u16::from_be_bytes([packet[EAPDU_OFFSET_P2], packet[EAPDU_OFFSET_P2 + 1]]);
        let request_id = u16::from_be_bytes([packet[EAPDU_OFFSET_LC], packet[EAPDU_OFFSET_LC + 1]]);

        if !is_valid_command(command) || total_packets == 0 || packet_sequence >= total_packets {
            return Err(RemoteWalletError::Protocol("Unable to parse packet header"));
        }

        Ok(Self {
            command,
            total_packets,
            packet_sequence,
            request_id,
        })
    }
}

/// Keystone hardware wallet device
pub struct KeystoneWallet {
    pub device: rusb::Device<rusb::Context>,
    pub handle: rusb::DeviceHandle<rusb::Context>,
    usb_io: UsbIo,
    pub pretty_path: String,
    pub version: Option<FirmwareVersion>,
    pub mfp: Option<[u8; 4]>,
}

impl fmt::Debug for KeystoneWallet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeystoneWallet")
    }
}

impl KeystoneWallet {
    pub fn new(
        device: rusb::Device<rusb::Context>,
        handle: rusb::DeviceHandle<rusb::Context>,
    ) -> Result<Self, RemoteWalletError> {
        let usb_io = Self::discover_usb_io(&device)?;

        // Best effort: detach kernel driver where supported.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            if handle
                .kernel_driver_active(usb_io.interface_number)
                .unwrap_or(false)
            {
                let _ = handle.detach_kernel_driver(usb_io.interface_number);
            }
        }

        handle
            .claim_interface(usb_io.interface_number)
            .map_err(|e| {
                RemoteWalletError::Hid(format!(
                    "Failed to claim USB interface {}: {e}",
                    usb_io.interface_number
                ))
            })?;

        Ok(Self {
            device,
            handle,
            usb_io,
            pretty_path: String::default(),
            version: None,
            mfp: None,
        })
    }

    fn discover_usb_io(device: &rusb::Device<rusb::Context>) -> Result<UsbIo, RemoteWalletError> {
        let config = device
            .active_config_descriptor()
            .or_else(|_| device.config_descriptor(0))
            .map_err(|e| {
                RemoteWalletError::Hid(format!("Failed to read USB config descriptor: {e}"))
            })?;

        // Match webusb-cli behavior first: interface 0, alternate setting 0, first IN/OUT endpoints.
        for interface in config.interfaces() {
            for descriptor in interface.descriptors() {
                if descriptor.interface_number() != 0 || descriptor.setting_number() != 0 {
                    continue;
                }
                for transfer_type in [rusb::TransferType::Bulk, rusb::TransferType::Interrupt] {
                    if let Some(endpoints) = find_endpoint_pair(&descriptor, transfer_type) {
                        return Ok(UsbIo {
                            interface_number: descriptor.interface_number(),
                            setting_number: descriptor.setting_number(),
                            endpoint_out: endpoints.output,
                            endpoint_in: endpoints.input,
                            transfer_type,
                        });
                    }
                }
            }
        }

        // Fallback: scan all interfaces/settings, BULK first, then INTERRUPT.
        for wanted_type in [rusb::TransferType::Bulk, rusb::TransferType::Interrupt] {
            for interface in config.interfaces() {
                for descriptor in interface.descriptors() {
                    if let Some(endpoints) = find_endpoint_pair(&descriptor, wanted_type) {
                        return Ok(UsbIo {
                            interface_number: descriptor.interface_number(),
                            setting_number: descriptor.setting_number(),
                            endpoint_out: endpoints.output,
                            endpoint_in: endpoints.input,
                            transfer_type: wanted_type,
                        });
                    }
                }
            }
        }

        Err(RemoteWalletError::Protocol(
            "No suitable USB IN/OUT endpoints found",
        ))
    }

    /// Write data to device with Keystone USB transport framing
    fn write(&self, command: CommandType, data: &[u8]) -> Result<(), RemoteWalletError> {
        // Avoid carrying unread responses across requests; stale data can keep
        // firmware IN endpoint busy and block subsequent sends.
        self.drain_pending_input_packets();

        let total_packets = std::cmp::max(1, data.len().div_ceil(EAPDU_MAX_REQ_DATA_PER_PACKET));

        for packet_index in 0..total_packets {
            let start = packet_index * EAPDU_MAX_REQ_DATA_PER_PACKET;
            let end = std::cmp::min(start + EAPDU_MAX_REQ_DATA_PER_PACKET, data.len());
            let chunk = &data[start..end];

            let mut eapdu_packet = [0u8; EAPDU_OFFSET_CDATA + EAPDU_MAX_REQ_DATA_PER_PACKET];
            eapdu_packet[EAPDU_OFFSET_CLA] = 0x00;
            eapdu_packet[EAPDU_OFFSET_INS..EAPDU_OFFSET_INS + 2]
                .copy_from_slice(&(command as u16).to_be_bytes());
            eapdu_packet[EAPDU_OFFSET_P1..EAPDU_OFFSET_P1 + 2]
                .copy_from_slice(&(total_packets as u16).to_be_bytes());
            eapdu_packet[EAPDU_OFFSET_P2..EAPDU_OFFSET_P2 + 2]
                .copy_from_slice(&(packet_index as u16).to_be_bytes());
            eapdu_packet[EAPDU_OFFSET_LC..EAPDU_OFFSET_LC + 2]
                .copy_from_slice(&REQUEST_ID.to_be_bytes());
            eapdu_packet[EAPDU_OFFSET_CDATA..EAPDU_OFFSET_CDATA + chunk.len()]
                .copy_from_slice(chunk);

            self.device_write(&eapdu_packet[..EAPDU_OFFSET_CDATA + chunk.len()])?;
        }

        Ok(())
    }

    /// Read data from device with Keystone USB transport parsing
    fn read(&self) -> Result<Vec<u8>, RemoteWalletError> {
        let mut total_packets: Option<u16> = None;
        let mut expected_req_id: Option<u16> = None;
        let mut expected_command: Option<u16> = None;
        let mut packet_chunks: Vec<Option<Vec<u8>>> = Vec::new();
        let mut response_status: Option<u16> = None;

        loop {
            let chunk = self.device_read()?;
            if chunk.is_empty() || chunk.iter().all(|b| *b == 0) {
                continue;
            }
            let packet = chunk.as_slice();
            let header = EapduHeader::parse(packet)?;
            let packet_payload = &packet[EAPDU_OFFSET_CDATA..];
            if packet_payload.len() < EAPDU_RESPONSE_STATUS_LEN {
                return Err(RemoteWalletError::Protocol("EAPDU payload too short"));
            }

            let payload_len = packet_payload.len() - EAPDU_RESPONSE_STATUS_LEN;
            let status =
                u16::from_be_bytes([packet_payload[payload_len], packet_payload[payload_len + 1]]);

            if total_packets.is_none() {
                total_packets = Some(header.total_packets);
                expected_req_id = Some(header.request_id);
                expected_command = Some(header.command);
                packet_chunks = vec![None; header.total_packets as usize];
            }

            if total_packets != Some(header.total_packets)
                || expected_req_id != Some(header.request_id)
                || expected_command != Some(header.command)
            {
                return Err(RemoteWalletError::Protocol(
                    "Mismatched EAPDU packet header across fragments",
                ));
            }

            let idx = header.packet_sequence as usize;
            if packet_chunks[idx].is_none() {
                packet_chunks[idx] = Some(packet_payload[..payload_len].to_vec());
            }

            if let Some(prev_status) = response_status {
                if prev_status != status {
                    return Err(RemoteWalletError::Protocol(
                        "Mismatched EAPDU status across fragments",
                    ));
                }
            } else {
                response_status = Some(status);
            }

            if packet_chunks.iter().all(|c| c.is_some()) {
                break;
            }
        }

        let result_len = packet_chunks
            .iter()
            .map(|chunk| chunk.as_ref().map_or(0, Vec::len))
            .sum();
        let mut result_data = Vec::with_capacity(result_len);
        for chunk in packet_chunks {
            result_data.extend_from_slice(&chunk.unwrap());
        }

        match response_status {
            Some(EAPDU_SUCCESS_STATUS) | None => {}
            Some(EAPDU_EXPORT_ADDRESS_PAGE_STATUS) => {
                return Err(RemoteWalletError::Protocol(ERROR_EXPORT_ADDRESS_PAGE));
            }
            Some(_) => {
                return Err(RemoteWalletError::Protocol(
                    "EAPDU returned non-success status",
                ));
            }
        }

        Ok(result_data)
    }

    /// Send APDU command and receive JSON response
    fn send_apdu(&self, command: CommandType, data: &[u8]) -> Result<String, RemoteWalletError> {
        self.write(command, data)?;
        let message = self.read()?;
        let message_str = String::from_utf8_lossy(&message);

        // Extract JSON from response
        if let (Some(start), Some(end)) = (message_str.find('{'), message_str.rfind('}')) {
            if start < end {
                let json_str = &message_str[start..=end];
                return Ok(json_str.to_string());
            }
        }

        Ok(message_str.to_string())
    }

    /// Get device firmware version and master fingerprint
    fn get_device_info(&self) -> Result<(FirmwareVersion, Option<[u8; 4]>), RemoteWalletError> {
        let json_str = self.send_apdu(CommandType::CmdGetDeviceInfo, &[])?;
        let json = serde_json::from_str::<serde_json::Value>(&json_str)
            .map_err(|_| RemoteWalletError::Protocol(ERROR_INVALID_JSON))?;

        if let Some(device_error) = json.get(JSON_FIELD_ERROR).and_then(|v| v.as_str()) {
            if !device_error.trim().is_empty() {
                return Err(RemoteWalletError::KeystoneError(device_error.to_string()));
            }
        }

        // Parse firmware version
        let version_str = json
            .get(JSON_FIELD_FIRMWARE_VERSION)
            .and_then(|v| v.as_str())
            .ok_or(RemoteWalletError::Protocol(ERROR_MISSING_FIELD))?;

        let version = FirmwareVersion::parse(version_str)
            .map_err(|_| RemoteWalletError::Protocol("Invalid firmware version"))?;

        // Parse master fingerprint (MFP)
        let mfp = json
            .get(JSON_FIELD_WALLET_MFP)
            .and_then(|v| v.as_str())
            .and_then(|hex_str| {
                let bytes = hex::decode(hex_str).ok()?;
                bytes.try_into().ok()
            });

        Ok((version, mfp))
    }

    /// Generate UR-encoded key derivation request for QR code display
    fn generate_hardware_call(
        &self,
        derivation_path: &DerivationPath,
    ) -> Result<String, RemoteWalletError> {
        let key_path = parse_crypto_key_path(derivation_path, self.mfp);
        let schema = KeyDerivationSchema::new(key_path, Some(Curve::Ed25519), None, None);
        let schemas = vec![schema];
        let call = QRHardwareCall::new(
            CallType::KeyDerivation,
            CallParams::KeyDerivation(KeyDerivationCall::new(schemas)),
            None,
            HardWareCallVersion::V1,
        );

        let bytes: Vec<u8> = call
            .try_into()
            .map_err(|_| RemoteWalletError::Protocol("Failed to encode QR call"))?;

        let encoded = probe_encode(
            &bytes,
            MAX_UR_FRAGMENT_LEN,
            QRHardwareCall::get_registry_type().get_type(),
        )
        .map_err(|_| RemoteWalletError::Protocol("Failed to encode UR"))?;

        Ok(encoded.data)
    }

    /// Generate UR-encoded sign request for transaction signing
    fn generate_sol_sign_request(
        &self,
        derivation_path: &DerivationPath,
        sign_data: &[u8],
    ) -> Result<String, RemoteWalletError> {
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

        let bytes: Vec<u8> = sol_sign_request
            .try_into()
            .map_err(|_| RemoteWalletError::Protocol("Failed to encode sign request"))?;

        let encoded = probe_encode(
            &bytes,
            MAX_UR_FRAGMENT_LEN,
            SolSignRequest::get_registry_type().get_type(),
        )
        .map_err(|_| RemoteWalletError::Protocol("Failed to encode UR"))?;

        Ok(encoded.data)
    }

    /// Low-level USB write to device
    fn device_write(&self, data: &[u8]) -> Result<(), RemoteWalletError> {
        match self.usb_io.transfer_type {
            rusb::TransferType::Interrupt => self
                .handle
                .write_interrupt(self.usb_io.endpoint_out, data, USB_TIMEOUT)
                .map_err(|e| RemoteWalletError::Hid(format!("USB write failed: {e}")))?,
            rusb::TransferType::Bulk => self
                .handle
                .write_bulk(self.usb_io.endpoint_out, data, USB_TIMEOUT)
                .map_err(|e| RemoteWalletError::Hid(format!("USB write failed: {e}")))?,
            _ => {
                return Err(RemoteWalletError::Protocol(
                    "Unsupported USB transfer type for write",
                ));
            }
        };

        Ok(())
    }

    fn drain_pending_input_packets(&self) {
        for _ in 0..MAX_DRAIN_PACKETS {
            match self.device_read_raw(DRAIN_TIMEOUT) {
                Ok(chunk) => {
                    if chunk.is_empty() {
                        break;
                    }
                }
                Err(rusb::Error::Timeout) => break,
                Err(rusb::Error::NotSupported) => break,
                Err(_) => break,
            }
        }
    }

    fn device_read_raw(&self, timeout: std::time::Duration) -> Result<Vec<u8>, rusb::Error> {
        let mut buf = vec![0u8; HID_PACKET_SIZE];

        let bytes_read = match self.usb_io.transfer_type {
            rusb::TransferType::Interrupt => {
                self.handle
                    .read_interrupt(self.usb_io.endpoint_in, &mut buf, timeout)?
            }
            rusb::TransferType::Bulk => {
                self.handle
                    .read_bulk(self.usb_io.endpoint_in, &mut buf, timeout)?
            }
            _ => return Err(rusb::Error::NotSupported),
        };

        buf.truncate(bytes_read);
        Ok(buf)
    }

    /// Low-level USB read from device
    fn device_read(&self) -> Result<Vec<u8>, RemoteWalletError> {
        self.device_read_raw(USB_TIMEOUT).map_err(|e| match e {
            rusb::Error::NotSupported => {
                RemoteWalletError::Protocol("Unsupported USB transfer type for read")
            }
            _ => RemoteWalletError::Hid(format!("USB read failed: {e}")),
        })
    }
}

fn find_endpoint_pair(
    descriptor: &rusb::InterfaceDescriptor<'_>,
    transfer_type: rusb::TransferType,
) -> Option<EndpointPair> {
    let mut endpoint_out = None;
    let mut endpoint_in = None;
    for ep in descriptor.endpoint_descriptors() {
        if ep.transfer_type() != transfer_type {
            continue;
        }
        match ep.direction() {
            rusb::Direction::Out => {
                endpoint_out.get_or_insert(ep.address());
            }
            rusb::Direction::In => {
                endpoint_in.get_or_insert(ep.address());
            }
        }
    }
    match (endpoint_out, endpoint_in) {
        (Some(output), Some(input)) => Some(EndpointPair { output, input }),
        _ => None,
    }
}

fn is_valid_command(value: u16) -> bool {
    matches!(value, 0x01..=0x06)
}

fn parse_ur_pubkey(ur: &str) -> Result<Vec<u8>, RemoteWalletError> {
    let result: ur_parse_lib::keystone_ur_decoder::URParseResult<CryptoMultiAccounts> =
        probe_decode(ur.to_lowercase())
            .map_err(|_| RemoteWalletError::Protocol("Failed to decode UR pubkey"))?;

    result
        .data
        .ok_or(RemoteWalletError::Protocol("No pubkey in response"))?
        .get_keys()
        .first()
        .ok_or(RemoteWalletError::Protocol("Empty pubkey list"))
        .map(|key| key.get_key())
}

fn parse_ur_signature(ur: &str) -> Result<Vec<u8>, RemoteWalletError> {
    let result: ur_parse_lib::keystone_ur_decoder::URParseResult<SolSignature> =
        probe_decode(ur.to_lowercase())
            .map_err(|_| RemoteWalletError::Protocol("Failed to decode UR signature"))?;

    Ok(result
        .data
        .ok_or(RemoteWalletError::Protocol("No signature in response"))?
        .get_signature()
        .as_slice()
        .to_vec())
}

/// Parse JSON field from response.
fn parse_json_field(json_str: &str, field_name: &str) -> Result<String, RemoteWalletError> {
    let json = serde_json::from_str::<serde_json::Value>(json_str)
        .map_err(|_| RemoteWalletError::Protocol(ERROR_INVALID_JSON))?;

    if let Some(device_error) = json.get(JSON_FIELD_ERROR).and_then(|v| v.as_str()) {
        if !device_error.trim().is_empty() {
            return Err(RemoteWalletError::KeystoneError(device_error.to_string()));
        }
    }

    json.get(field_name)
        .and_then(|v| v.as_str())
        .ok_or(RemoteWalletError::Protocol(ERROR_MISSING_FIELD))
        .map(String::from)
}

impl RemoteWallet<rusb::Device<rusb::Context>> for KeystoneWallet {
    fn name(&self) -> &str {
        "Keystone hardware wallet"
    }

    fn read_device(
        &mut self,
        _dev_info: &rusb::Device<rusb::Context>,
    ) -> Result<RemoteWalletInfo, RemoteWalletError> {
        // Get device info (firmware version and MFP)
        let (version, mfp) = self.get_device_info()?;
        self.version = Some(version);
        self.mfp = mfp;

        // Get device descriptor for model and serial
        let device_descriptor = self
            .device
            .device_descriptor()
            .map_err(|e| RemoteWalletError::Hid(format!("Failed to get device descriptor: {e}")))?;

        let model = format!(
            "Keystone {:04x}:{:04x}",
            device_descriptor.vendor_id(),
            device_descriptor.product_id()
        );

        let serial = self
            .handle
            .read_serial_number_string_ascii(&device_descriptor)
            .unwrap_or_else(|_| "Unknown".to_string());

        // Try to get default pubkey
        let default_path = DerivationPath::new_bip44(Some(0), Some(0));
        let pubkey_result = self.get_pubkey(&default_path, false);
        let (pubkey, error) = match pubkey_result {
            Ok(pubkey) => (pubkey, None),
            Err(err) => (Pubkey::default(), Some(err)),
        };

        let mut info = RemoteWalletInfo {
            model,
            manufacturer: Manufacturer::Keystone,
            serial,
            host_device_path: String::new(),
            pubkey,
            error,
        };
        info.host_device_path = info.get_pretty_path();

        Ok(info)
    }

    fn get_pubkey(
        &self,
        derivation_path: &DerivationPath,
        _confirm_key: bool,
    ) -> Result<Pubkey, RemoteWalletError> {
        let use_cached_usb_pubkey = is_path_in_cached_range(derivation_path);

        let pubkey_bytes = if use_cached_usb_pubkey {
            let serialized_path = extend_and_serialize(derivation_path);
            let json_response =
                self.send_apdu(CommandType::CmdGetDeviceUSBPubkey, &serialized_path)?;
            let pubkey_hex = parse_json_field(&json_response, JSON_FIELD_PUBKEY)
                .or_else(|_| parse_json_field(&json_response, JSON_FIELD_PAYLOAD))?;
            hex::decode(pubkey_hex).map_err(|_| RemoteWalletError::Protocol(ERROR_INVALID_HEX))?
        } else {
            let ur_request = self.generate_hardware_call(derivation_path)?;
            let json_response = self.send_apdu(CommandType::CmdResolveUR, ur_request.as_bytes())?;
            let pubkey_ur = parse_json_field(&json_response, JSON_FIELD_PAYLOAD)?;
            if pubkey_ur.trim().is_empty() {
                return Err(RemoteWalletError::Protocol(
                    "CmdResolveUR returned empty payload",
                ));
            }
            parse_ur_pubkey(&pubkey_ur)?
        };

        Pubkey::try_from(pubkey_bytes).map_err(|_| RemoteWalletError::Protocol(ERROR_KEY_SIZE))
    }

    fn sign_message(
        &self,
        derivation_path: &DerivationPath,
        data: &[u8],
    ) -> Result<Signature, RemoteWalletError> {
        let ur_request = self.generate_sol_sign_request(derivation_path, data)?;

        println!(
            "Waiting for your approval on {} {}",
            self.name(),
            self.pretty_path
        );

        let json_response = self.send_apdu(CommandType::CmdResolveUR, ur_request.as_bytes())?;

        let signature_ur = parse_json_field(&json_response, JSON_FIELD_PAYLOAD)?;
        let signature_bytes = parse_ur_signature(&signature_ur)?;
        println!("{CHECK_MARK}Approved");

        Signature::try_from(signature_bytes)
            .map_err(|_| RemoteWalletError::Protocol(ERROR_SIGNATURE_SIZE))
    }

    fn sign_offchain_message(
        &self,
        derivation_path: &DerivationPath,
        message: &[u8],
    ) -> Result<Signature, RemoteWalletError> {
        self.sign_message(derivation_path, message)
    }
}

/// Check if device is a Keystone
pub fn is_valid_keystone(vendor_id: u16, product_id: u16) -> bool {
    vendor_id == KEYSTONE_VID && product_id == KEYSTONE_PID
}

fn extend_and_serialize(derivation_path: &DerivationPath) -> Vec<u8> {
    let path = derivation_path.path();
    // Firmware expects: [coin_type(4 bytes, BE)] [depth(1 byte)] [path components(4 bytes each, BE)]
    let mut serialized = Vec::with_capacity(4 + 1 + path.len() * 4);
    serialized.extend_from_slice(&SOLANA_COIN_TYPE.to_be_bytes());
    serialized.push(path.len() as u8);
    for index in path {
        serialized.extend_from_slice(&index.to_bits().to_be_bytes());
    }
    serialized
}

/// Check whether a derivation path can be served by Keystone's pre-cached usb pubkey range.
/// 44'/501'
/// 44'/501'/0' ... 44'/501'/49'
/// 44'/501'/0'/0 ... 44'/501'/49'/0
fn is_path_in_cached_range(derivation_path: &DerivationPath) -> bool {
    let path = derivation_path.path();
    if path.len() < 2 {
        return false;
    }

    let purpose = path[0].to_bits() & 0x7fff_ffff;
    let coin = path[1].to_bits() & 0x7fff_ffff;
    if purpose != 44 || coin != 501 {
        return false;
    }

    match path.len() {
        2 => true, // m/44'/501'
        3 => {
            let account = path[2].to_bits() & 0x7fff_ffff;
            account <= CACHED_ACCOUNT_RANGE
        }
        4 => {
            let account = path[2].to_bits() & 0x7fff_ffff;
            let change = path[3].to_bits() & 0x7fff_ffff;
            change == CACHED_FIXED_CHANGE && account <= CACHED_ACCOUNT_RANGE
        }
        _ => false,
    }
}

/// Parse derivation path into CryptoKeyPath for UR encoding
fn parse_crypto_key_path(derivation_path: &DerivationPath, mfp: Option<[u8; 4]>) -> CryptoKeyPath {
    let mut path_components = Vec::new();

    for index in derivation_path.path() {
        let bits = index.to_bits();
        let hardened = (bits & 0x8000_0000) != 0;
        let value = bits & 0x7fff_ffff;
        if let Ok(component) = PathComponent::new(Some(value), hardened) {
            path_components.push(component);
        }
    }

    if path_components.is_empty() {
        if let Ok(component) = PathComponent::new(Some(44u32), true) {
            path_components.push(component);
        }
        if let Ok(component) = PathComponent::new(Some(501u32), true) {
            path_components.push(component);
        }
        if let Ok(component) = PathComponent::new(Some(0u32), true) {
            path_components.push(component);
        }
    }

    CryptoKeyPath::new(path_components, mfp, None)
}
