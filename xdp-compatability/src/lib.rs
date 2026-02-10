//! Helpers for XDP compatibility client/server.
//! Request: PREFIX + seq + token.
//! Response: RESPONSE_PREFIX + seq + hash(PREFIX || token).

pub const PREFIX: &[u8] = b"agave-xdp:";
pub const RESPONSE_PREFIX: &[u8] = b"agave-xdp-resp:";
pub const SEQ_SIZE: usize = std::mem::size_of::<u64>();
const SEQ_OFFSET: usize = PREFIX.len();
const TOKEN_OFFSET: usize = SEQ_OFFSET + SEQ_SIZE;
pub const PAYLOAD_SIZE: usize = TOKEN_OFFSET + SEQ_SIZE;

/// Length of the response (SHA-256).
pub const HASH_RESPONSE_LEN: usize = 32;
const RESPONSE_SEQ_OFFSET: usize = RESPONSE_PREFIX.len();
const RESPONSE_HASH_OFFSET: usize = RESPONSE_SEQ_OFFSET + SEQ_SIZE;
pub const RESPONSE_LEN: usize = RESPONSE_HASH_OFFSET + HASH_RESPONSE_LEN;

/// Returns the token for a given sequence number.
pub fn make_token(seq: u64) -> u64 {
    let hash = solana_sha256_hasher::hashv(&[PREFIX, &seq.to_be_bytes()]);
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&hash.as_ref()[..8]);
    u64::from_be_bytes(bytes)
}

/// Returns hash of the token bytes
pub fn hash_token(token: &[u8]) -> [u8; HASH_RESPONSE_LEN] {
    let h = solana_sha256_hasher::hashv(&[PREFIX, token]);
    let mut out = [0u8; HASH_RESPONSE_LEN];
    out.copy_from_slice(h.as_ref());
    out
}

/// For a valid agave-xdp request, returns hash of the token to send back.
pub fn hash_response(buf: &[u8], len: usize) -> Option<[u8; HASH_RESPONSE_LEN]> {
    if len != PAYLOAD_SIZE || !buf[..len].starts_with(PREFIX) {
        return None;
    }
    Some(hash_token(&buf[TOKEN_OFFSET..PAYLOAD_SIZE]))
}

pub fn response_for_request(buf: &[u8], len: usize) -> Option<[u8; RESPONSE_LEN]> {
    let hash = hash_response(buf, len)?;
    let mut out = [0u8; RESPONSE_LEN];
    out[..RESPONSE_PREFIX.len()].copy_from_slice(RESPONSE_PREFIX);
    out[RESPONSE_SEQ_OFFSET..RESPONSE_HASH_OFFSET].copy_from_slice(&buf[SEQ_OFFSET..TOKEN_OFFSET]);
    out[RESPONSE_HASH_OFFSET..].copy_from_slice(&hash);
    Some(out)
}

pub fn response_seq(buf: &[u8], len: usize) -> Option<u64> {
    if len != RESPONSE_LEN || !buf[..len].starts_with(RESPONSE_PREFIX) {
        return None;
    }
    let mut seq = [0u8; SEQ_SIZE];
    seq.copy_from_slice(&buf[RESPONSE_SEQ_OFFSET..RESPONSE_HASH_OFFSET]);
    Some(u64::from_be_bytes(seq))
}

/// Builds a request payload with the given seq/token.
pub fn build_request(seq: u64, token: u64) -> Vec<u8> {
    let mut payload = Vec::with_capacity(PAYLOAD_SIZE);
    payload.extend_from_slice(PREFIX);
    payload.extend_from_slice(&seq.to_be_bytes());
    payload.extend_from_slice(&token.to_be_bytes());
    payload
}

/// Returns the response bytes expected for a request.
pub fn expected_response(request: &[u8]) -> Option<[u8; RESPONSE_LEN]> {
    response_for_request(request, request.len())
}
