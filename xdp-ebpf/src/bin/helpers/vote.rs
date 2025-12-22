use {
    crate::helpers::{ExtractedHeader, Reject, Verdict},
    agave_xdp_ebpf::RejectReason,
};

const SIGNATURE_LEN: usize = 64;
const PUBKEY_LEN: usize = 32;
/// Simple vote transactions are signed by the vote authority and possibly the fee payer.
const VOTE_SIGNATURE_COUNTS: core::ops::RangeInclusive<u8> = 1..=2;

/// Checks that the packet could be a vote transaction. `first_byte` is the short_vec encoded
/// signature count, which is a single byte for counts below 128.
#[inline(always)]
pub fn check_vote_signature(header: &ExtractedHeader, first_byte: Option<u8>) -> Verdict {
    let too_short = Reject::new(RejectReason::VoteTooShort, header.payload_len as u16);
    let sig_count = first_byte.ok_or(too_short)?;
    if !VOTE_SIGNATURE_COUNTS.contains(&sig_count) {
        return Err(Reject::new(
            RejectReason::VoteSignatureCount,
            sig_count.into(),
        ));
    }
    // signature count, signatures and at least one account key
    #[allow(clippy::arithmetic_side_effects)] // sig_count is at most 2
    let min_len = 1 + usize::from(sig_count) * SIGNATURE_LEN + PUBKEY_LEN;
    if header.payload_len < min_len {
        return Err(too_short);
    }
    Ok(())
}
