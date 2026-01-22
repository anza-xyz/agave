//! Shared protocol for XDP compatibility client/server.
//! Request: PREFIX + big-endian u64 token + padding.
//! Response: SHA-256 hash of the token (32 bytes), via solana_sha256_hasher::hashv.

use std::mem;

pub const PREFIX: &[u8] = b"agave-xdp:";

const fn token_offset() -> usize {
    PREFIX.len()
}

const fn token_size() -> usize {
    mem::size_of::<u64>()
}

pub const MIN_PAYLOAD: usize = token_offset() + token_size();

/// Length of the response (SHA-256 digest).
pub const HASH_RESPONSE_LEN: usize = 32;

pub fn is_agave_xdp_request(buf: &[u8], len: usize) -> bool {
    len >= MIN_PAYLOAD && buf[..PREFIX.len()] == *PREFIX
}

/// Returns SHA-256 hash of the token bytes (domain-separated with PREFIX).
pub fn hash_token(token: &[u8]) -> [u8; HASH_RESPONSE_LEN] {
    let h = solana_sha256_hasher::hashv(&[PREFIX, token]);
    let mut out = [0u8; HASH_RESPONSE_LEN];
    out.copy_from_slice(h.as_ref());
    out
}

/// For a valid agave-xdp request, returns the 32-byte hash of the token to send back.
pub fn hash_response(buf: &[u8], len: usize) -> Option<[u8; HASH_RESPONSE_LEN]> {
    if !is_agave_xdp_request(buf, len) {
        return None;
    }
    Some(hash_token(&buf[token_offset()..MIN_PAYLOAD]))
}

/// Builds a request payload with the given token and at least `payload_size` bytes (padded with b'x').
pub fn build_request(token: u64, payload_size: usize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(payload_size.max(MIN_PAYLOAD));
    payload.extend_from_slice(PREFIX);
    payload.extend_from_slice(&token.to_be_bytes());
    if payload.len() < payload_size {
        payload.resize(payload_size, b'x');
    }
    payload
}

/// Returns the expected response bytes (SHA-256 hash of the token in the request).
pub fn expected_response(request: &[u8]) -> Vec<u8> {
    if request.len() < MIN_PAYLOAD {
        return Vec::new();
    }
    hash_token(&request[token_offset()..MIN_PAYLOAD]).to_vec()
}
