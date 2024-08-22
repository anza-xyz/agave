use {
    log::*,
    solana_perf::sigverify::PacketError,
    solana_sdk::{packet::Packet, short_vec::decode_shortu16_len, signature::SIGNATURE_BYTES},
};

// TODO: copied from transaction-status crate, better to reuse?
/// Get the signature of the transaction packet
/// This does a rudimentry verification to make sure the packet at least
/// contains the signature data and it returns the reference to the signature.
pub fn get_signature_from_packet(packet: &Packet) -> Result<&[u8; SIGNATURE_BYTES], PacketError> {
    let (sig_len_untrusted, sig_start) = packet
        .data(..)
        .and_then(|bytes| decode_shortu16_len(bytes).ok())
        .ok_or(PacketError::InvalidShortVec)?;

    if sig_len_untrusted < 1 {
        return Err(PacketError::InvalidSignatureLen);
    }

    let signature = packet
        .data(sig_start..sig_start.saturating_add(SIGNATURE_BYTES))
        .ok_or(PacketError::InvalidSignatureLen)?;
    let signature = signature
        .try_into()
        .map_err(|_| PacketError::InvalidSignatureLen)?;
    Ok(signature)
}

pub fn should_trace(packet: &Packet) -> bool {
	// TODO make conditional
	true
}

pub fn trace_packet(checkpoint: &'static str, packet: &Packet) {
    info!(
        "trace packet: checkpoint:{} sig:{} ip:{} sender-key:{}",
        checkpoint,
        get_signature_from_packet(packet).map(|b| bs58::encode(b).into_string()).unwrap_or("<ERR>".into()),
        packet.meta().addr.to_string(),
        packet.meta().remote_pubkey.map(|b| bs58::encode(b).into_string()).unwrap_or("<NONE>".into()),
    );
}

pub fn maybe_trace_packet(checkpoint: &'static str, packet: &Packet) {
    if should_trace(packet) {
        trace_packet(checkpoint, packet);
    }
}
