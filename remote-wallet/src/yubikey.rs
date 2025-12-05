//! YubiKey PIV hardware wallet support for Solana.
//!
//! Supports Ed25519 signing via YubiKey's PIV application (firmware 5.7+).

use {
    crate::{
        locator::Manufacturer,
        remote_wallet::{RemoteWallet, RemoteWalletError, RemoteWalletInfo},
    },
    console::Emoji,
    log::*,
    pcsc::{Card, Context, Protocols, Scope, ShareMode, MAX_BUFFER_SIZE},
    solana_derivation_path::DerivationPath,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    std::{cell::RefCell, fmt, str::FromStr},
    thiserror::Error,
};

static CHECK_MARK: Emoji<'_, '_> = Emoji("", "");

/// PIV Application ID
const PIV_AID: &[u8] = &[0xA0, 0x00, 0x00, 0x03, 0x08];

/// CLA byte values (per ISO 7816-4 / yubico-piv-tool)
const CLA_ISO: u8 = 0x00;
/// Chain bit mask - set on all but last chunk when command chaining
const CLA_CHAIN: u8 = 0x10;

/// APDU instruction codes (per yubico-piv-tool)
const INS_SELECT: u8 = 0xA4;
const INS_VERIFY: u8 = 0x20;
const INS_AUTHENTICATE: u8 = 0x87;
const INS_GET_METADATA: u8 = 0xF7;
const INS_GET_SERIAL: u8 = 0xF8;
/// GET RESPONSE - retrieve continuation data when SW1=0x61
const INS_GET_RESPONSE: u8 = 0xC0;

/// Algorithm identifiers
const ALGO_ED25519: u8 = 0xE0;

/// Status words
const SW_SUCCESS: u16 = 0x9000;
const SW_SECURITY_STATUS: u16 = 0x6982;
const SW_AUTH_BLOCKED: u16 = 0x6983;
const SW_CONDITIONS_NOT_SATISFIED: u16 = 0x6985;
const SW_WRONG_DATA: u16 = 0x6A80;
const SW_WRONG_PIN_BASE: u16 = 0x63C0;
const SW_FILE_NOT_FOUND: u16 = 0x6A82;
const SW_REFERENCE_NOT_FOUND: u16 = 0x6A88;

/// SW1 byte indicating wrong Le - SW2 contains correct Le value
const SW1_WRONG_LE: u8 = 0x6C;
/// SW1 byte indicating more data available - SW2 contains bytes remaining
const SW1_MORE_DATA: u8 = 0x61;

/// PIV PIN reference
const PIV_PIN_REF: u8 = 0x80;

/// TLV tags for GET METADATA response
const TAG_ALGORITHM: u8 = 0x01;
/// Combined PIN/touch policy (two bytes: pin, touch)
const TAG_POLICY: u8 = 0x02;
/// Origin tag (generated/imported) – currently ignored
const TAG_ORIGIN: u8 = 0x03;
const TAG_PUBLIC_KEY: u8 = 0x04;

/// TLV tags for GENERAL AUTHENTICATE (Dynamic Authentication Template)
const TAG_DYNAMIC_AUTH_TEMPLATE: u8 = 0x7C;
/// Response/signature tag within Dynamic Authentication Template
const TAG_AUTH_RESPONSE: u8 = 0x82;
/// Challenge/data tag within Dynamic Authentication Template
const TAG_AUTH_CHALLENGE: u8 = 0x81;

/// TLV tag for ECC point (public key) in metadata response
const TAG_ECC_POINT: u8 = 0x86;

/// PIV key slot.
///
/// Valid slots are:
/// - 0x9A: PIV Authentication
/// - 0x9C: Digital Signature (default)
/// - 0x9D: Key Management
/// - 0x9E: Card Authentication
/// - 0x82-0x95: Retired Key Management slots
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PivSlot(u8);

impl PivSlot {
    /// PIV Authentication slot (9a)
    pub const AUTHENTICATION: Self = Self(0x9A);
    /// Digital Signature slot (9c) - default for Solana
    pub const SIGNATURE: Self = Self(0x9C);
    /// Key Management slot (9d)
    pub const KEY_MANAGEMENT: Self = Self(0x9D);
    /// Card Authentication slot (9e)
    pub const CARD_AUTH: Self = Self(0x9E);

    /// Create a new PivSlot from a byte value.
    ///
    /// Returns an error if the byte is not a valid PIV slot.
    pub fn new(byte: u8) -> Result<Self, YubiKeyError> {
        match byte {
            0x9A | 0x9C | 0x9D | 0x9E => Ok(Self(byte)),
            0x82..=0x95 => Ok(Self(byte)),
            _ => Err(YubiKeyError::InvalidSlot(format!("{:02x}", byte))),
        }
    }
}

impl Default for PivSlot {
    fn default() -> Self {
        Self::SIGNATURE
    }
}

impl From<PivSlot> for u8 {
    fn from(slot: PivSlot) -> Self {
        slot.0
    }
}

impl fmt::Display for PivSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}", self.0)
    }
}

/// YubiKey-specific errors
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum YubiKeyError {
    #[error("PIV application not available on this YubiKey")]
    PivNotAvailable,

    #[error("PIN verification required")]
    SecurityStatus,

    #[error("PIN blocked (too many failed attempts). Reset with: ykman piv access unblock")]
    AuthBlocked,

    #[error("Touch required - please touch your YubiKey")]
    TouchRequired,

    #[error("Invalid PIN ({0} attempts remaining)")]
    WrongPin(u8),

    #[error(
        "No Ed25519 key in slot {0}. Generate with: ykman piv keys generate -a ED25519 {0} \
         pubkey.pem"
    )]
    SlotEmpty(PivSlot),

    #[error(
        "Key in slot {0} is not Ed25519 (found algorithm 0x{1:02X}). Solana requires Ed25519 keys."
    )]
    WrongAlgorithm(PivSlot, u8),

    #[error("Derivation paths not supported by YubiKey PIV. Use ?slot=XX to select key slot.")]
    DerivationPathNotSupported,

    #[error("PCSC daemon not running. On Linux: sudo systemctl start pcscd")]
    PcscDaemonUnavailable,

    #[error("PCSC error: {0}")]
    Pcsc(#[source] pcsc::Error),

    #[error("No YubiKey found. Ensure device is connected.")]
    NoDeviceFound,

    #[error("YubiKey with serial {0} not found")]
    DeviceNotFound(u32),

    #[error("Communication error: {0}")]
    Protocol(String),

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error("Invalid slot: {0}")]
    InvalidSlot(String),
}

impl From<pcsc::Error> for YubiKeyError {
    fn from(err: pcsc::Error) -> Self {
        match err {
            pcsc::Error::NoService | pcsc::Error::ServiceStopped => {
                YubiKeyError::PcscDaemonUnavailable
            }
            pcsc::Error::NoReadersAvailable | pcsc::Error::ReaderUnavailable => {
                YubiKeyError::NoDeviceFound
            }
            _ => YubiKeyError::Pcsc(err),
        }
    }
}

impl TryFrom<u8> for PivSlot {
    type Error = YubiKeyError;

    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        Self::new(byte)
    }
}

impl FromStr for PivSlot {
    type Err = YubiKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let byte =
            u8::from_str_radix(s, 16).map_err(|_| YubiKeyError::InvalidSlot(s.to_string()))?;
        Self::new(byte).map_err(|_| YubiKeyError::InvalidSlot(s.to_string()))
    }
}

/// PIN policy for a key slot
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinPolicy {
    /// PIN never required
    Never = 1,
    /// PIN required once per session
    Once = 2,
    /// PIN required for each operation
    Always = 3,
}

impl TryFrom<u8> for PinPolicy {
    type Error = YubiKeyError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(PinPolicy::Never),
            2 => Ok(PinPolicy::Once),
            3 => Ok(PinPolicy::Always),
            _ => Err(YubiKeyError::InvalidResponse(format!(
                "Unknown PIN policy: {value}"
            ))),
        }
    }
}

/// Touch policy for a key slot
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPolicy {
    /// Touch never required
    Never = 1,
    /// Touch always required
    Always = 2,
    /// Touch required, cached for 15 seconds
    Cached = 3,
}

impl TryFrom<u8> for TouchPolicy {
    type Error = YubiKeyError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(TouchPolicy::Never),
            2 => Ok(TouchPolicy::Always),
            3 => Ok(TouchPolicy::Cached),
            _ => Err(YubiKeyError::InvalidResponse(format!(
                "Unknown touch policy: {value}"
            ))),
        }
    }
}

/// Metadata for a PIV key slot
#[derive(Debug, Clone)]
pub struct SlotMetadata {
    pub algorithm: u8,
    pub pin_policy: PinPolicy,
    pub touch_policy: TouchPolicy,
    pub public_key: Option<Vec<u8>>,
}

/// Low-level PIV card interface
pub struct PivCard {
    card: Card,
    pin_verified: bool,
}

impl PivCard {
    /// Create a new PivCard from a connected smart card
    pub fn from_card(card: Card) -> Self {
        Self {
            card,
            pin_verified: false,
        }
    }

    /// Check if PIN has been verified this session
    pub fn is_pin_verified(&self) -> bool {
        self.pin_verified
    }

    /// Send an APDU command and receive response
    fn send_apdu(
        &self,
        cla: u8,
        ins: u8,
        p1: u8,
        p2: u8,
        data: &[u8],
    ) -> Result<Vec<u8>, YubiKeyError> {
        const MAX_CHUNK_SIZE: usize = 255;

        let mut chunks = data.chunks(MAX_CHUNK_SIZE);
        let Some(last_chunk) = chunks.next_back() else {
            // Empty data - send single APDU
            return self.send_apdu_single(cla, ins, p1, p2, data);
        };

        // Process intermediate chunks with chain bit set (per Yubico reference)
        for chunk in chunks {
            let response = self.send_apdu_single(cla | CLA_CHAIN, ins, p1, p2, chunk)?;
            if !response.is_empty() {
                log::debug!(
                    "Unexpected data in chained APDU response: {:02X?}",
                    response
                );
            }
        }

        // Last chunk - expect response
        self.send_apdu_single(cla, ins, p1, p2, last_chunk)
    }

    /// Send a single APDU command (no chaining)
    ///
    /// Handles ISO 7816-4 status word conventions:
    /// - SW1=0x6C (wrong Le): resends command with correct Le from SW2
    /// - SW1=0x61 (more data): issues GET RESPONSE commands to retrieve all data
    fn send_apdu_single(
        &self,
        cla: u8,
        ins: u8,
        p1: u8,
        p2: u8,
        data: &[u8],
    ) -> Result<Vec<u8>, YubiKeyError> {
        let mut command = vec![cla, ins, p1, p2];

        if !data.is_empty() {
            command.push(data.len() as u8);
            command.extend_from_slice(data);
        }

        // Add Le (expected response length) - 0x00 means max (256 bytes)
        command.push(0x00);

        let mut response_buf = [0u8; MAX_BUFFER_SIZE];
        let mut response = self.card.transmit(&command, &mut response_buf)?;

        if response.len() < 2 {
            return Err(YubiKeyError::InvalidResponse("Response too short".into()));
        }

        let mut sw1 = response[response.len() - 2];
        let mut sw2 = response[response.len() - 1];

        // Handle "wrong Le" (SW1=0x6C) - resend with correct Le from SW2
        if sw1 == SW1_WRONG_LE {
            *command.last_mut().unwrap() = sw2; // Replace Le with correct value
            response = self.card.transmit(&command, &mut response_buf)?;

            if response.len() < 2 {
                return Err(YubiKeyError::InvalidResponse("Response too short".into()));
            }
            sw1 = response[response.len() - 2];
            sw2 = response[response.len() - 1];
        }

        // Collect response data (excluding status word)
        let mut result = response[..response.len() - 2].to_vec();

        // Handle "more data available" (SW1=0x61) - loop with GET RESPONSE
        while sw1 == SW1_MORE_DATA {
            // GET RESPONSE: CLA=00, INS=C0, P1=00, P2=00, Le=SW2 (or 0x00 for 256)
            let le = if sw2 == 0 { 0x00 } else { sw2 };
            let get_response_cmd = [CLA_ISO, INS_GET_RESPONSE, 0x00, 0x00, le];
            response = self.card.transmit(&get_response_cmd, &mut response_buf)?;

            if response.len() < 2 {
                return Err(YubiKeyError::InvalidResponse(
                    "GET RESPONSE too short".into(),
                ));
            }

            sw1 = response[response.len() - 2];
            sw2 = response[response.len() - 1];

            // Append data (excluding status word)
            result.extend_from_slice(&response[..response.len() - 2]);
        }

        // Final status word check
        let sw = ((sw1 as u16) << 8) | (sw2 as u16);
        match sw {
            SW_SUCCESS => Ok(result),
            SW_SECURITY_STATUS => Err(YubiKeyError::SecurityStatus),
            SW_AUTH_BLOCKED => Err(YubiKeyError::AuthBlocked),
            SW_CONDITIONS_NOT_SATISFIED => Err(YubiKeyError::TouchRequired),
            SW_WRONG_DATA | SW_FILE_NOT_FOUND | SW_REFERENCE_NOT_FOUND => {
                Err(YubiKeyError::InvalidResponse(format!("Status: 0x{sw:04X}")))
            }
            sw if (sw & 0xFFF0) == SW_WRONG_PIN_BASE => {
                let retries = (sw & 0x0F) as u8;
                Err(YubiKeyError::WrongPin(retries))
            }
            _ => Err(YubiKeyError::Protocol(format!("Status: 0x{sw:04X}"))),
        }
    }

    /// Select the PIV application
    pub fn select_piv(&mut self) -> Result<(), YubiKeyError> {
        self.send_apdu(CLA_ISO, INS_SELECT, 0x04, 0x00, PIV_AID)?;
        self.pin_verified = false;
        Ok(())
    }

    /// Verify PIN
    pub fn verify_pin(&mut self, pin: &str) -> Result<(), YubiKeyError> {
        // PIN must be padded to 8 bytes with 0xFF
        let mut pin_bytes = [0xFF; 8];
        let pin_data = pin.as_bytes();
        if pin_data.len() > 8 {
            return Err(YubiKeyError::Protocol("PIN too long".into()));
        }
        pin_bytes[..pin_data.len()].copy_from_slice(pin_data);

        self.send_apdu(CLA_ISO, INS_VERIFY, 0x00, PIV_PIN_REF, &pin_bytes)?;
        self.pin_verified = true;
        Ok(())
    }

    /// Get device serial number
    pub fn get_serial(&self) -> Result<u32, YubiKeyError> {
        let response = self.send_apdu(CLA_ISO, INS_GET_SERIAL, 0x00, 0x00, &[])?;
        if response.len() != 4 {
            return Err(YubiKeyError::InvalidResponse(format!(
                "Expected 4 bytes for serial, got {}",
                response.len()
            )));
        }
        Ok(u32::from_be_bytes([
            response[0],
            response[1],
            response[2],
            response[3],
        ]))
    }

    /// Get metadata for a slot
    pub fn get_metadata(&self, slot: PivSlot) -> Result<SlotMetadata, YubiKeyError> {
        let response = self.send_apdu(CLA_ISO, INS_GET_METADATA, 0x00, u8::from(slot), &[])?;

        let mut algorithm = 0u8;
        let mut pin_policy = PinPolicy::Once;
        let mut touch_policy = TouchPolicy::Never;
        let mut public_key = None;

        // Parse TLV response
        let mut data = response.as_slice();
        while !data.is_empty() {
            let (tag, value, rest) = parse_tlv(data)?;
            data = rest;

            match tag {
                TAG_ALGORITHM => {
                    if !value.is_empty() {
                        algorithm = value[0];
                    }
                }
                TAG_POLICY => {
                    if value.len() < 2 {
                        return Err(YubiKeyError::InvalidResponse("Policy TLV too short".into()));
                    }
                    pin_policy = PinPolicy::try_from(value[0])?;
                    touch_policy = TouchPolicy::try_from(value[1])?;
                }
                TAG_ORIGIN => {}
                TAG_PUBLIC_KEY => {
                    public_key = Some(value.to_vec());
                }
                _ => {
                    // Skip unknown tags
                }
            }
        }

        Ok(SlotMetadata {
            algorithm,
            pin_policy,
            touch_policy,
            public_key,
        })
    }

    /// Sign data using Ed25519
    ///
    /// Uses GENERAL AUTHENTICATE command (INS 0x87) with Dynamic Authentication Template.
    ///
    /// Request TLV structure (per Yubico PIV reference):
    /// ```text
    ///   7C [len]              - Dynamic Authentication Template
    ///     82 00               - Response tag (empty = request signature)
    ///     81 [len] [data]     - Challenge/data to sign
    /// ```
    ///
    /// Response TLV structure:
    /// ```text
    ///   7C [len]              - Dynamic Authentication Template
    ///     82 [len] [signature] - Signature bytes (64 bytes for Ed25519)
    /// ```
    ///
    /// For command chaining (data > 255 bytes), the CLA chain bit (0x10) is set
    /// on all but the last chunk per Yubico reference.
    pub fn sign(&self, slot: PivSlot, data: &[u8]) -> Result<Vec<u8>, YubiKeyError> {
        let mut auth_data = Vec::new();

        // Response tag (empty = request signature)
        auth_data.push(TAG_AUTH_RESPONSE);
        auth_data.push(0x00);

        // Challenge/data to sign
        auth_data.push(TAG_AUTH_CHALLENGE);
        encode_tlv_length(data.len(), &mut auth_data);
        auth_data.extend_from_slice(data);

        // Wrap in Dynamic Authentication Template
        let mut command_data = vec![TAG_DYNAMIC_AUTH_TEMPLATE];
        encode_tlv_length(auth_data.len(), &mut command_data);
        command_data.extend_from_slice(&auth_data);

        let response = self.send_apdu(
            CLA_ISO,
            INS_AUTHENTICATE,
            ALGO_ED25519,
            u8::from(slot),
            &command_data,
        )?;

        // Parse response - should be 7C L 82 L signature
        parse_signature_response(&response)
    }
}

/// Encode a TLV length value using DER encoding rules
fn encode_tlv_length(len: usize, out: &mut Vec<u8>) {
    if len < 128 {
        out.push(len as u8);
    } else if len < 256 {
        out.push(0x81);
        out.push(len as u8);
    } else {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push((len & 0xFF) as u8);
    }
}

/// Extract Ed25519 public key from metadata response.
///
/// Per yubico-piv-tool, the public key is wrapped with tag 0x86 (ECC point).
/// Expected format: `86 [length] <32 bytes>`
fn extract_ed25519_pubkey(data: &[u8]) -> Option<Pubkey> {
    const ED25519_PUBKEY_LEN: usize = 32;

    // Check for ECC point tag (0x86) wrapper
    if data.first() == Some(&TAG_ECC_POINT) {
        let (len, len_bytes) = parse_tlv_len(&data[1..]).ok()?;
        if len != ED25519_PUBKEY_LEN {
            return None;
        }
        let start = 1 + len_bytes;
        return Pubkey::try_from(data.get(start..start + len)?).ok();
    }

    // Raw 32-byte key without wrapper
    if data.len() == ED25519_PUBKEY_LEN {
        return Pubkey::try_from(data).ok();
    }

    None
}

/// Parse signature response from AUTHENTICATE command.
///
/// Response format: `7C [len] 82 [len] [signature]`
fn parse_signature_response(response: &[u8]) -> Result<Vec<u8>, YubiKeyError> {
    // Parse outer Dynamic Authentication Template
    let (tag, value, _) = parse_tlv(response)?;
    if tag != TAG_DYNAMIC_AUTH_TEMPLATE {
        return Err(YubiKeyError::InvalidResponse(
            "Expected Dynamic Authentication Template".into(),
        ));
    }

    // Parse inner signature tag
    let (tag, signature, _) = parse_tlv(value)?;
    if tag != TAG_AUTH_RESPONSE {
        return Err(YubiKeyError::InvalidResponse(
            "Expected signature tag 0x82".into(),
        ));
    }

    Ok(signature.to_vec())
}

/// Parse TLV length encoding and return (length, bytes_consumed).
///
/// Handles BER-TLV length encoding:
/// - 0x00-0x7F: single byte length
/// - 0x81 NN: one additional byte
/// - 0x82 NN NN: two additional bytes (big-endian)
fn parse_tlv_len(data: &[u8]) -> Result<(usize, usize), YubiKeyError> {
    let first = *data
        .first()
        .ok_or_else(|| YubiKeyError::InvalidResponse("Empty TLV length".into()))?;

    if first < 0x80 {
        // Short form: length is the byte itself
        Ok((first as usize, 1))
    } else if first == 0x81 {
        // One additional byte for length
        let len = *data
            .get(1)
            .ok_or_else(|| YubiKeyError::InvalidResponse("TLV length truncated".into()))?;
        Ok((len as usize, 2))
    } else if first == 0x82 {
        // Two additional bytes for length (big-endian)
        if data.len() < 3 {
            return Err(YubiKeyError::InvalidResponse("TLV length truncated".into()));
        }
        let len = ((data[1] as usize) << 8) | (data[2] as usize);
        Ok((len, 3))
    } else {
        Err(YubiKeyError::InvalidResponse(format!(
            "Unsupported length encoding: 0x{first:02X}"
        )))
    }
}

/// Parse a single TLV element and return (tag, value, rest).
///
/// This is the ergonomic helper for parsing TLV sequences - it handles
/// the tag byte, length parsing, and slicing in one call.
fn parse_tlv(input: &[u8]) -> Result<(u8, &[u8], &[u8]), YubiKeyError> {
    let tag = *input
        .first()
        .ok_or_else(|| YubiKeyError::InvalidResponse("Empty TLV".into()))?;

    let (len, len_bytes) = parse_tlv_len(&input[1..])?;
    let start = 1 + len_bytes;
    let end = start + len;

    let value = input
        .get(start..end)
        .ok_or_else(|| YubiKeyError::InvalidResponse("TLV value truncated".into()))?;

    let rest = &input[end..];
    Ok((tag, value, rest))
}

// ============================================================================
// YubiKeyWallet - RemoteWallet Implementation
// ============================================================================

/// Device info for PCSC-connected devices
pub struct PcscDeviceInfo {
    /// Reader name from PCSC
    pub reader_name: String,
    /// Device serial number (if available)
    pub serial: Option<u32>,
}

/// YubiKey hardware wallet
pub struct YubiKeyWallet {
    card: RefCell<PivCard>,
    serial: u32,
}

impl fmt::Debug for YubiKeyWallet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("YubiKeyWallet")
            .field("serial", &self.serial)
            .finish()
    }
}

impl YubiKeyWallet {
    /// Create a new YubiKeyWallet
    pub fn new(card: PivCard, serial: u32) -> Self {
        Self {
            card: RefCell::new(card),
            serial,
        }
    }

    /// Get the device serial number
    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// Check if PIN verification is needed and prompt if necessary
    fn ensure_pin_verified(&self) -> Result<(), YubiKeyError> {
        let card = self.card.borrow();
        if card.is_pin_verified() {
            return Ok(());
        }
        drop(card);

        // Prompt for PIN
        let pin = rpassword::prompt_password("Enter YubiKey PIV PIN: ")
            .map_err(|e| YubiKeyError::Protocol(format!("Failed to read PIN: {e}")))?;

        let mut card = self.card.borrow_mut();
        card.verify_pin(&pin)?;
        Ok(())
    }

    /// Print touch reminder if touch policy requires it
    fn print_touch_reminder(&self, metadata: &SlotMetadata) {
        match metadata.touch_policy {
            TouchPolicy::Never => {}
            TouchPolicy::Always => {
                eprintln!("Touch your YubiKey to confirm...");
            }
            TouchPolicy::Cached => {
                // With cached policy, touch may or may not be required depending on
                // whether the 15-second cache has expired. We show a hint since there's
                // no APDU-level way to detect if touch will actually be needed.
                eprintln!("Touch your YubiKey if prompted...");
            }
        }
    }

    /// Check that derivation path is default (YubiKey doesn't support derivation)
    fn check_derivation_path(&self, derivation_path: &DerivationPath) -> Result<(), YubiKeyError> {
        if derivation_path != &DerivationPath::default() {
            return Err(YubiKeyError::DerivationPathNotSupported);
        }
        Ok(())
    }

    /// Get the public key for a specific slot
    ///
    /// This method allows selecting which PIV slot to read the key from,
    /// similar to how derivation_path selects keys on Ledger/Trezor.
    pub fn get_pubkey_for_slot(
        &self,
        slot: PivSlot,
        confirm_key: bool,
    ) -> Result<Pubkey, RemoteWalletError> {
        let card = self.card.borrow();

        // Get metadata to verify key type
        let metadata = card.get_metadata(slot).map_err(|e| {
            if matches!(e, YubiKeyError::InvalidResponse(_)) {
                YubiKeyError::SlotEmpty(slot)
            } else {
                e
            }
        })?;

        if metadata.algorithm != ALGO_ED25519 {
            return Err(YubiKeyError::WrongAlgorithm(slot, metadata.algorithm).into());
        }

        let pk = metadata
            .public_key
            .ok_or_else(|| RemoteWalletError::Protocol("No public key in slot metadata"))?;

        let pubkey = extract_ed25519_pubkey(&pk).ok_or_else(|| {
            log::debug!(
                "Public key data from metadata ({} bytes): {:02X?}",
                pk.len(),
                pk
            );
            RemoteWalletError::Protocol("Could not extract Ed25519 public key from metadata")
        })?;

        if confirm_key {
            println!(
                "Public key from YubiKey slot {}: {}{CHECK_MARK}",
                slot, pubkey
            );
        }

        Ok(pubkey)
    }

    /// Sign a message using a specific slot
    ///
    /// This method allows selecting which PIV slot to sign with,
    /// similar to how derivation_path selects keys on Ledger/Trezor.
    pub fn sign_message_for_slot(
        &self,
        slot: PivSlot,
        data: &[u8],
    ) -> Result<Signature, RemoteWalletError> {
        // Get metadata to check policies
        let card = self.card.borrow();
        let metadata = card.get_metadata(slot)?;
        drop(card);

        // Verify PIN if required
        if metadata.pin_policy != PinPolicy::Never {
            self.ensure_pin_verified()?;
        }

        // Print touch reminder
        self.print_touch_reminder(&metadata);

        // Sign
        let card = self.card.borrow();
        let sig_bytes = card.sign(slot, data)?;

        if sig_bytes.len() != 64 {
            return Err(RemoteWalletError::Protocol(
                "Signature packet size mismatch",
            ));
        }

        Signature::try_from(sig_bytes.as_slice())
            .map_err(|_| RemoteWalletError::Protocol("Invalid signature"))
    }
}

impl RemoteWallet<PcscDeviceInfo> for YubiKeyWallet {
    fn name(&self) -> &str {
        "YubiKey hardware wallet"
    }

    fn read_device(
        &mut self,
        _dev_info: &PcscDeviceInfo,
    ) -> Result<RemoteWalletInfo, RemoteWalletError> {
        // Don't probe for keys during enumeration. The slot to use is not known yet
        // (user may specify ?slot=9a etc.), and probing only the default slot (9c)
        // would cause enumeration to fail if 9c is empty, blocking access to keys
        // in other slots. Key retrieval happens later via get_pubkey_for_slot().
        Ok(RemoteWalletInfo {
            model: "YubiKey".to_string(),
            manufacturer: Manufacturer::YubiKey,
            serial: self.serial.to_string(),
            host_device_path: format!("usb://yubikey?serial={}", self.serial),
            pubkey: Pubkey::default(),
            error: None,
        })
    }

    fn get_pubkey(
        &self,
        derivation_path: &DerivationPath,
        confirm_key: bool,
    ) -> Result<Pubkey, RemoteWalletError> {
        self.check_derivation_path(derivation_path)?;
        self.get_pubkey_for_slot(PivSlot::default(), confirm_key)
    }

    fn sign_message(
        &self,
        derivation_path: &DerivationPath,
        data: &[u8],
    ) -> Result<Signature, RemoteWalletError> {
        self.check_derivation_path(derivation_path)?;
        self.sign_message_for_slot(PivSlot::default(), data)
    }

    fn sign_offchain_message(
        &self,
        derivation_path: &DerivationPath,
        message: &[u8],
    ) -> Result<Signature, RemoteWalletError> {
        // For off-chain messages, use the same signing mechanism
        self.sign_message(derivation_path, message)
    }
}

/// Check if a reader name indicates a YubiKey device
pub fn is_yubikey_reader(reader_name: &str) -> bool {
    let name_lower = reader_name.to_lowercase();
    name_lower.contains("yubikey") || name_lower.contains("yubico")
}

/// Establish PCSC context
pub fn establish_context() -> Result<Context, YubiKeyError> {
    Context::establish(Scope::User).map_err(|e| e.into())
}

/// List all YubiKey readers
pub fn list_yubikey_readers(ctx: &Context) -> Result<Vec<String>, YubiKeyError> {
    let mut readers_buf = [0u8; 2048];
    let readers = ctx.list_readers(&mut readers_buf)?;

    Ok(readers
        .filter(|r| is_yubikey_reader(&r.to_string_lossy()))
        .map(|r| r.to_string_lossy().into_owned())
        .collect())
}

/// Connect to a YubiKey and create a wallet
pub fn connect_yubikey(ctx: &Context, reader_name: &str) -> Result<YubiKeyWallet, YubiKeyError> {
    let reader = std::ffi::CString::new(reader_name)
        .map_err(|_| YubiKeyError::Protocol("Invalid reader name".into()))?;

    let card = ctx.connect(&reader, ShareMode::Shared, Protocols::ANY)?;
    let mut piv_card = PivCard::from_card(card);
    piv_card.select_piv()?;

    let serial = piv_card.get_serial()?;

    Ok(YubiKeyWallet::new(piv_card, serial))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_piv_slot_from_str() {
        // Standard slots - case insensitive
        assert_eq!(PivSlot::from_str("9a").unwrap(), PivSlot::AUTHENTICATION);
        assert_eq!(PivSlot::from_str("9A").unwrap(), PivSlot::AUTHENTICATION);
        assert_eq!(PivSlot::from_str("9c").unwrap(), PivSlot::SIGNATURE);
        assert_eq!(PivSlot::from_str("9d").unwrap(), PivSlot::KEY_MANAGEMENT);
        assert_eq!(PivSlot::from_str("9e").unwrap(), PivSlot::CARD_AUTH);
        // Retired slots
        assert_eq!(u8::from(PivSlot::from_str("82").unwrap()), 0x82);
        assert_eq!(u8::from(PivSlot::from_str("95").unwrap()), 0x95);
        // Invalid slots
        assert!(PivSlot::from_str("81").is_err());
        assert!(PivSlot::from_str("96").is_err());
        assert!(PivSlot::from_str("invalid").is_err());
    }

    #[test]
    fn test_piv_slot_to_u8() {
        assert_eq!(u8::from(PivSlot::AUTHENTICATION), 0x9A);
        assert_eq!(u8::from(PivSlot::SIGNATURE), 0x9C);
        assert_eq!(u8::from(PivSlot::KEY_MANAGEMENT), 0x9D);
        assert_eq!(u8::from(PivSlot::CARD_AUTH), 0x9E);
        assert_eq!(u8::from(PivSlot::new(0x82).unwrap()), 0x82);
    }

    #[test]
    fn test_piv_slot_try_from() {
        assert_eq!(PivSlot::try_from(0x9A).unwrap(), PivSlot::AUTHENTICATION);
        assert_eq!(PivSlot::try_from(0x9C).unwrap(), PivSlot::SIGNATURE);
        assert_eq!(
            PivSlot::try_from(0x82).unwrap(),
            PivSlot::new(0x82).unwrap()
        );
        assert!(PivSlot::try_from(0x81).is_err());
        assert!(PivSlot::try_from(0x96).is_err());
        assert!(PivSlot::try_from(0x00).is_err());
    }

    #[test]
    fn test_piv_slot_display() {
        assert_eq!(PivSlot::AUTHENTICATION.to_string(), "9a");
        assert_eq!(PivSlot::SIGNATURE.to_string(), "9c");
        assert_eq!(PivSlot::new(0x82).unwrap().to_string(), "82");
    }

    #[test]
    fn test_is_yubikey_reader() {
        assert!(is_yubikey_reader("Yubico YubiKey OTP+FIDO+CCID 00"));
        assert!(is_yubikey_reader("yubikey"));
        assert!(is_yubikey_reader("YUBIKEY"));
        assert!(is_yubikey_reader("Yubico Something"));
        assert!(!is_yubikey_reader("Generic Smart Card Reader"));
        assert!(!is_yubikey_reader("Ledger Nano S"));
    }

    #[test]
    fn test_parse_tlv_len() {
        // Short form (0x00-0x7F)
        let (len, len_bytes) = parse_tlv_len(&[0x20, 0x01, 0x02]).unwrap();
        assert_eq!(len, 0x20);
        assert_eq!(len_bytes, 1);

        // One byte length (0x81 NN)
        let (len, len_bytes) = parse_tlv_len(&[0x81, 0x80, 0x01]).unwrap();
        assert_eq!(len, 0x80);
        assert_eq!(len_bytes, 2);

        // Two byte length (0x82 NN NN)
        let (len, len_bytes) = parse_tlv_len(&[0x82, 0x01, 0x00, 0x01]).unwrap();
        assert_eq!(len, 0x0100);
        assert_eq!(len_bytes, 3);

        // Empty data
        assert!(parse_tlv_len(&[]).is_err());

        // Truncated one-byte length
        assert!(parse_tlv_len(&[0x81]).is_err());

        // Truncated two-byte length
        assert!(parse_tlv_len(&[0x82, 0x01]).is_err());
    }

    #[test]
    fn test_parse_tlv() {
        // Simple TLV: tag=0x01, len=0x02, value=[0xAA, 0xBB], rest=[0xCC]
        let data = [0x01, 0x02, 0xAA, 0xBB, 0xCC];
        let (tag, value, rest) = parse_tlv(&data).unwrap();
        assert_eq!(tag, 0x01);
        assert_eq!(value, &[0xAA, 0xBB]);
        assert_eq!(rest, &[0xCC]);

        // TLV with one-byte length encoding
        let mut data = vec![0x04, 0x81, 0x80]; // tag=0x04, len=128
        data.extend(vec![0x55; 128]); // 128 bytes of value
        data.push(0xFF); // rest
        let (tag, value, rest) = parse_tlv(&data).unwrap();
        assert_eq!(tag, 0x04);
        assert_eq!(value.len(), 128);
        assert_eq!(rest, &[0xFF]);

        // Empty input
        assert!(parse_tlv(&[]).is_err());

        // Truncated value
        assert!(parse_tlv(&[0x01, 0x05, 0xAA]).is_err());
    }

    #[test]
    fn test_pin_policy_from_u8() {
        assert_eq!(PinPolicy::try_from(1).unwrap(), PinPolicy::Never);
        assert_eq!(PinPolicy::try_from(2).unwrap(), PinPolicy::Once);
        assert_eq!(PinPolicy::try_from(3).unwrap(), PinPolicy::Always);
        assert!(PinPolicy::try_from(0).is_err());
        assert!(PinPolicy::try_from(4).is_err());
    }

    #[test]
    fn test_touch_policy_from_u8() {
        assert_eq!(TouchPolicy::try_from(1).unwrap(), TouchPolicy::Never);
        assert_eq!(TouchPolicy::try_from(2).unwrap(), TouchPolicy::Always);
        assert_eq!(TouchPolicy::try_from(3).unwrap(), TouchPolicy::Cached);
        assert!(TouchPolicy::try_from(0).is_err());
        assert!(TouchPolicy::try_from(4).is_err());
    }
}
