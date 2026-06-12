use {
    crate::byzzfuzz::interceptor::{AlpenglowInterceptAction, AlpenglowRng},
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::{Block, ConsensusMessage, VoteMessage},
        vote::Vote,
    },
    rand::Rng,
    solana_bls_signatures::{Signature as BLSSignature, keypair::Keypair as BLSKeypair},
    solana_clock::Slot,
    solana_hash::Hash,
};

const MIN_DELAY_MESSAGES: usize = 1;
const MAX_DELAY_MESSAGES: usize = 16;

pub(crate) fn maybe_mutate_alpenglow_message(
    message: &ConsensusMessage,
    rng: &mut AlpenglowRng,
    source_bls_keypair: Option<&BLSKeypair>,
) -> Option<AlpenglowInterceptAction> {
    if rng.random_range(0..100) >= 50 {
        return None;
    }
    let mutation = Mutations::ALL[rng.random_range(0..Mutations::ALL.len())];
    mutation.apply(message, rng, source_bls_keypair)
}

#[derive(Clone, Copy, Debug)]
pub enum Mutations {
    DropMessage,
    DuplicateMessage,
    DuplicateToAll,
    DelayMessage,
    RankBitFlip,
    WrongRank,
    NotarizeEquivocation,
    FallbackEquivocation,
    FastFallbackSwap,
    CertBitmapBitflip,
    CertBitmapClear,
    CertBitmapSet,
    CertSignatureBitflip,
    CertTypeSwap,
    CertSlotPlus1,
    CertSlotMinus1,
    EquivocateVote,
    ParentSlotPlus1,
    ParentSlotMinus1,
    BlockIdBitflip,
}

impl Mutations {
    const ALL: &'static [Self] = &[
        Self::DropMessage,
        Self::DuplicateMessage,
        Self::DuplicateToAll,
        Self::DelayMessage,
        Self::RankBitFlip,
        Self::WrongRank,
        Self::NotarizeEquivocation,
        Self::FallbackEquivocation,
        Self::FastFallbackSwap,
        Self::CertBitmapBitflip,
        Self::CertBitmapClear,
        Self::CertBitmapSet,
        Self::CertSignatureBitflip,
        Self::CertTypeSwap,
        Self::CertSlotPlus1,
        Self::CertSlotMinus1,
        Self::EquivocateVote,
        Self::ParentSlotPlus1,
        Self::ParentSlotMinus1,
        Self::BlockIdBitflip,
    ];

    fn apply(
        self,
        message: &ConsensusMessage,
        rng: &mut AlpenglowRng,
        source_bls_keypair: Option<&BLSKeypair>,
    ) -> Option<AlpenglowInterceptAction> {
        match self {
            Self::DropMessage => Some(AlpenglowInterceptAction::Drop),
            Self::DuplicateMessage => Some(AlpenglowInterceptAction::Duplicate),
            Self::DuplicateToAll => Some(AlpenglowInterceptAction::DuplicateToAll),
            Self::DelayMessage => Some(AlpenglowInterceptAction::DelayMessages(
                rng.random_range(MIN_DELAY_MESSAGES..=MAX_DELAY_MESSAGES),
            )),
            _ => self
                .mutate_message(message, rng, source_bls_keypair)
                .map(|message| AlpenglowInterceptAction::Replace(Box::new(message))),
        }
    }

    fn mutate_message(
        self,
        message: &ConsensusMessage,
        rng: &mut AlpenglowRng,
        source_bls_keypair: Option<&BLSKeypair>,
    ) -> Option<ConsensusMessage> {
        match message {
            ConsensusMessage::Certificate(cert) => self
                .mutate_certificate(cert, rng)
                .map(ConsensusMessage::Certificate),
            ConsensusMessage::Vote(vote) => self
                .mutate_vote_message(vote, rng, source_bls_keypair)
                .map(ConsensusMessage::Vote),
        }
    }

    fn mutate_certificate(self, cert: &Certificate, rng: &mut AlpenglowRng) -> Option<Certificate> {
        let mut mutated = cert.clone();
        match self {
            Self::CertSlotPlus1 => {
                mutated.cert_type = shift_cert_slot(cert.cert_type, SlotShift::Plus(1));
            }
            Self::CertSlotMinus1 => {
                mutated.cert_type = shift_cert_slot(cert.cert_type, SlotShift::Minus(1));
            }
            Self::CertBitmapBitflip => bitflip_bytes(&mut mutated.bitmap, rng)?,
            Self::CertBitmapClear => clear_bitmap_bit(&mut mutated.bitmap, rng)?,
            Self::CertBitmapSet => set_bitmap_bit(&mut mutated.bitmap, rng),
            Self::CertSignatureBitflip => bitflip_signature(&mut mutated.signature, rng),
            Self::CertTypeSwap => mutated.cert_type = swap_cert_type(cert.cert_type),
            _ => {
                // Keep signatures/bitmaps intact so validators reject changed payloads.
                mutated.cert_type =
                    mutate_cert_block(cert.cert_type, |block| self.mutate_block(block, rng))??;
            }
        }
        Some(mutated)
    }

    fn mutate_vote_message(
        self,
        vote_message: &VoteMessage,
        rng: &mut AlpenglowRng,
        source_bls_keypair: Option<&BLSKeypair>,
    ) -> Option<VoteMessage> {
        // Only byzantine sources reach here, and re-signing needs their key.
        let source_bls_keypair = source_bls_keypair?;
        let mut mutated = vote_message.clone();
        match self {
            Self::RankBitFlip => bitflip_rank(&mut mutated.rank, rng),
            Self::WrongRank => mutated.rank ^= 1,
            _ => {
                mutated.vote = match self {
                    Self::EquivocateVote => equivocate_vote(vote_message.vote, rng),
                    Self::NotarizeEquivocation => notarize_equivocation(vote_message.vote, rng),
                    Self::FallbackEquivocation => fallback_equivocation(vote_message.vote, rng),
                    Self::FastFallbackSwap => fast_fallback_swap(vote_message.vote),
                    _ => {
                        mutate_vote_block(vote_message.vote, |block| self.mutate_block(block, rng))
                    }
                }?;
            }
        }
        // Re-sign mutated votes as the sending validator.
        mutated.signature = source_bls_keypair
            .sign(wincode::serialize(&mutated.vote).ok()?.as_slice())
            .into();
        Some(mutated)
    }

    fn mutate_block(self, mut block: Block, rng: &mut AlpenglowRng) -> Option<Block> {
        // Alpenglow block payloads do not carry an explicit parent slot.
        match self {
            Self::ParentSlotPlus1 => block.slot = block.slot.saturating_add(1),
            Self::ParentSlotMinus1 => block.slot = block.slot.saturating_sub(1),
            Self::BlockIdBitflip => block.block_id = bitflip_hash(block.block_id, rng),
            _ => return None,
        }
        Some(block)
    }
}

enum SlotShift {
    Plus(Slot),
    Minus(Slot),
}

fn shift_cert_slot(cert_type: CertificateType, shift: SlotShift) -> CertificateType {
    let slot = shift_slot(cert_type.slot(), shift);
    match cert_type {
        CertificateType::Finalize(_) => CertificateType::Finalize(slot),
        CertificateType::Skip(_) => CertificateType::Skip(slot),
        CertificateType::FinalizeFast(mut block) => {
            block.slot = slot;
            CertificateType::FinalizeFast(block)
        }
        CertificateType::Notarize(mut block) => {
            block.slot = slot;
            CertificateType::Notarize(block)
        }
        CertificateType::NotarizeFallback(mut block) => {
            block.slot = slot;
            CertificateType::NotarizeFallback(block)
        }
        CertificateType::Genesis(mut block) => {
            block.slot = slot;
            CertificateType::Genesis(block)
        }
    }
}

fn mutate_cert_block(
    cert_type: CertificateType,
    mutate: impl FnOnce(Block) -> Option<Block>,
) -> Option<Option<CertificateType>> {
    let cert_type = match cert_type {
        CertificateType::FinalizeFast(block) => CertificateType::FinalizeFast(mutate(block)?),
        CertificateType::Notarize(block) => CertificateType::Notarize(mutate(block)?),
        CertificateType::NotarizeFallback(block) => {
            CertificateType::NotarizeFallback(mutate(block)?)
        }
        CertificateType::Genesis(block) => CertificateType::Genesis(mutate(block)?),
        CertificateType::Finalize(_) | CertificateType::Skip(_) => return Some(None),
    };
    Some(Some(cert_type))
}

fn mutate_vote_block(vote: Vote, mutate: impl FnOnce(Block) -> Option<Block>) -> Option<Vote> {
    match vote {
        Vote::Notarize(mut vote) => {
            vote.block = mutate(vote.block)?;
            Some(Vote::Notarize(vote))
        }
        Vote::NotarizeFallback(mut vote) => {
            vote.block = mutate(vote.block)?;
            Some(Vote::NotarizeFallback(vote))
        }
        Vote::Genesis(mut vote) => {
            vote.block = mutate(vote.block)?;
            Some(Vote::Genesis(vote))
        }
        Vote::Finalize(_) | Vote::Skip(_) | Vote::SkipFallback(_) => None,
    }
}

fn bitflip_rank(rank: &mut u16, rng: &mut AlpenglowRng) {
    let bit = rng.random_range(0..16);
    *rank ^= 1u16 << bit;
}

fn equivocate_vote(vote: Vote, rng: &mut AlpenglowRng) -> Option<Vote> {
    match vote {
        Vote::Notarize(vote) => Some(Vote::new_skip_vote(vote.block.slot)),
        Vote::Skip(vote) => Some(Vote::new_notarization_vote(random_block(vote.slot, rng))),
        Vote::NotarizeFallback(vote) => Some(Vote::new_skip_fallback_vote(vote.block.slot)),
        Vote::SkipFallback(vote) => Some(Vote::new_notarization_fallback_vote(random_block(
            vote.slot, rng,
        ))),
        Vote::Finalize(_) | Vote::Genesis(_) => None,
    }
}

fn notarize_equivocation(vote: Vote, rng: &mut AlpenglowRng) -> Option<Vote> {
    match vote {
        Vote::Notarize(mut vote) => {
            vote.block.block_id = bitflip_hash(vote.block.block_id, rng);
            Some(Vote::Notarize(vote))
        }
        _ => None,
    }
}

fn fallback_equivocation(vote: Vote, rng: &mut AlpenglowRng) -> Option<Vote> {
    match vote {
        Vote::NotarizeFallback(mut vote) => {
            vote.block.block_id = bitflip_hash(vote.block.block_id, rng);
            Some(Vote::NotarizeFallback(vote))
        }
        _ => None,
    }
}

fn fast_fallback_swap(vote: Vote) -> Option<Vote> {
    match vote {
        Vote::Notarize(vote) => Some(Vote::new_notarization_fallback_vote(vote.block)),
        Vote::NotarizeFallback(vote) => Some(Vote::new_notarization_vote(vote.block)),
        Vote::Skip(vote) => Some(Vote::new_skip_fallback_vote(vote.slot)),
        Vote::SkipFallback(vote) => Some(Vote::new_skip_vote(vote.slot)),
        Vote::Finalize(_) | Vote::Genesis(_) => None,
    }
}

fn swap_cert_type(cert_type: CertificateType) -> CertificateType {
    match cert_type {
        CertificateType::Finalize(slot) => CertificateType::Skip(slot),
        CertificateType::Skip(slot) => CertificateType::Finalize(slot),
        CertificateType::FinalizeFast(block) => CertificateType::Notarize(block),
        CertificateType::Notarize(block) => CertificateType::NotarizeFallback(block),
        CertificateType::NotarizeFallback(block) => CertificateType::Notarize(block),
        CertificateType::Genesis(block) => CertificateType::Notarize(block),
    }
}

fn random_block(slot: Slot, rng: &mut AlpenglowRng) -> Block {
    Block {
        slot,
        block_id: Hash::new_from_array(rng.random()),
    }
}

fn bitflip_bytes(bytes: &mut [u8], rng: &mut AlpenglowRng) -> Option<()> {
    if bytes.is_empty() {
        return None;
    }
    let byte = rng.random_range(0..bytes.len());
    let bit = rng.random_range(0..8);
    bytes[byte] ^= 1u8 << bit;
    Some(())
}

fn clear_bitmap_bit(bitmap: &mut [u8], rng: &mut AlpenglowRng) -> Option<()> {
    let byte = random_nonzero_byte(bitmap, rng)?;
    let bits = (0..8)
        .filter(|bit| bitmap[byte] & (1u8 << bit) != 0)
        .collect::<Vec<_>>();
    let bit = bits[rng.random_range(0..bits.len())];
    bitmap[byte] &= !(1u8 << bit);
    Some(())
}

fn set_bitmap_bit(bitmap: &mut Vec<u8>, rng: &mut AlpenglowRng) {
    let byte = match bitmap.iter().position(|byte| *byte != u8::MAX) {
        Some(byte) => byte,
        None => {
            bitmap.push(0);
            bitmap.len().saturating_sub(1)
        }
    };
    let bits = (0..8)
        .filter(|bit| bitmap[byte] & (1u8 << bit) == 0)
        .collect::<Vec<_>>();
    let bit = bits[rng.random_range(0..bits.len())];
    bitmap[byte] |= 1u8 << bit;
}

fn random_nonzero_byte(bytes: &[u8], rng: &mut AlpenglowRng) -> Option<usize> {
    let bytes = bytes
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte != 0).then_some(index))
        .collect::<Vec<_>>();
    (!bytes.is_empty()).then(|| bytes[rng.random_range(0..bytes.len())])
}

fn bitflip_signature(signature: &mut BLSSignature, rng: &mut AlpenglowRng) {
    let byte = rng.random_range(0..signature.0.len());
    let bit = rng.random_range(0..8);
    signature.0[byte] ^= 1u8 << bit;
}

fn bitflip_hash(hash: Hash, rng: &mut AlpenglowRng) -> Hash {
    let mut bytes = hash.to_bytes();
    let byte = rng.random_range(0..bytes.len());
    let bit = rng.random_range(0..8);
    bytes[byte] ^= 1u8 << bit;
    Hash::new_from_array(bytes)
}

fn shift_slot(slot: Slot, shift: SlotShift) -> Slot {
    match shift {
        SlotShift::Plus(amount) => slot.saturating_add(amount),
        SlotShift::Minus(amount) => slot.saturating_sub(amount),
    }
}
