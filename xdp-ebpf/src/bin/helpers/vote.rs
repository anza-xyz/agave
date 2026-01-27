use {crate::helpers::ExtractedHeader, agave_xdp_ebpf::FirewallDecision};

// we never have > 127 bytes in votes =)
fn decode_shortu16_len(val: u8) -> Option<u8> {
    if val < 127 {
        Some(val as u8)
    } else {
        None
    }
}

const SIG_LEN: usize = 64;

pub const PUBKEY_LENGTH: usize = 32;

pub fn check_vote_signature(header: &ExtractedHeader, first_byte: u8) -> FirewallDecision {
    // should have at least 1 signature and 1 program ID + sig lengths
    if header.payload_len < SIG_LEN + PUBKEY_LENGTH + 1 {
        return FirewallDecision::VoteTooShort;
    }
    // read the length of Transaction.signatures (serialized with short_vec)
    let sig_len = decode_shortu16_len(first_byte).unwrap_or_default();

    // vote could have 1 or 2 sigs
    if !(1u8..=2).contains(&sig_len) {
        return FirewallDecision::VoteInvalidSigCount(sig_len);
    }
    // after the sigs we must be able to still fit the program ID at the very least
    let msg_start_offset = sig_len as usize * SIG_LEN + 1;
    if msg_start_offset + PUBKEY_LENGTH >= header.payload_len {
        return FirewallDecision::VoteTooShort;
    }
    FirewallDecision::Pass
}
