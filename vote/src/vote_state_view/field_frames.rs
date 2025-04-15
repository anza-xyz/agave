use {
    super::{list_view::ListView, Result, VoteStateViewError},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    std::io::BufRead,
};

pub(super) trait ListFrame {
    type Item;

    // SAFETY: Each implementor MUST enforce that `Self::Item` is alignment 1 to
    // ensure that after casting it won't have alignment issues, any heap
    // allocated fields, or any assumptions about endianness.
    #[cfg(test)]
    const ASSERT_ITEM_ALIGNMENT: ();

    fn len(&self) -> usize;
    fn item_size(&self) -> usize {
        core::mem::size_of::<Self::Item>()
    }

    /// This function is safe under the following conditions:
    /// SAFETY:
    /// - `Self::Item` is alignment 1
    /// - The passed `item_data` slice is large enough for the type `Self::Item`
    /// - `Self::Item` is valid for any sequence of bytes
    unsafe fn read_item<'a>(&self, item_data: &'a [u8]) -> &'a Self::Item {
        &*(item_data.as_ptr() as *const Self::Item)
    }

    fn total_size(&self) -> usize {
        core::mem::size_of::<u64>() /* len */ + self.total_item_size()
    }

    fn total_item_size(&self) -> usize {
        self.len() * self.item_size()
    }
}

pub(super) enum VotesFrame {
    Lockout(LockoutListFrame),
    Landed(LandedVotesListFrame),
}

impl ListFrame for VotesFrame {
    type Item = LockoutItem;

    #[cfg(test)]
    const ASSERT_ITEM_ALIGNMENT: () = {
        static_assertions::const_assert!(core::mem::align_of::<LockoutItem>() == 1);
    };

    fn len(&self) -> usize {
        match self {
            Self::Lockout(frame) => frame.len(),
            Self::Landed(frame) => frame.len(),
        }
    }

    fn item_size(&self) -> usize {
        match self {
            Self::Lockout(frame) => frame.item_size(),
            Self::Landed(frame) => frame.item_size(),
        }
    }

    unsafe fn read_item<'a>(&self, item_data: &'a [u8]) -> &'a Self::Item {
        match self {
            Self::Lockout(frame) => frame.read_item(item_data),
            Self::Landed(frame) => frame.read_item(item_data),
        }
    }
}

#[repr(C)]
pub(super) struct LockoutItem {
    slot: [u8; 8],
    confirmation_count: [u8; 4],
}

impl LockoutItem {
    #[inline]
    pub(super) fn slot(&self) -> Slot {
        u64::from_le_bytes(self.slot)
    }
    #[inline]
    pub(super) fn confirmation_count(&self) -> u32 {
        u32::from_le_bytes(self.confirmation_count)
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
pub(super) struct LockoutListFrame {
    pub(super) len: u8,
}

impl LockoutListFrame {
    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) -> Result<Self> {
        let len = solana_serialize_utils::cursor::read_u64(cursor)
            .map_err(|_err| VoteStateViewError::AccountDataTooSmall)? as usize;
        let len = u8::try_from(len).map_err(|_| VoteStateViewError::InvalidVotesLength)?;
        let frame = Self { len };
        cursor.consume(frame.total_item_size());
        Ok(frame)
    }
}

impl ListFrame for LockoutListFrame {
    type Item = LockoutItem;

    #[cfg(test)]
    const ASSERT_ITEM_ALIGNMENT: () = {
        static_assertions::const_assert!(core::mem::align_of::<LockoutItem>() == 1);
    };

    fn len(&self) -> usize {
        self.len as usize
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
pub(super) struct LandedVotesListFrame {
    pub(super) len: u8,
}

impl LandedVotesListFrame {
    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) -> Result<Self> {
        let len = solana_serialize_utils::cursor::read_u64(cursor)
            .map_err(|_err| VoteStateViewError::AccountDataTooSmall)? as usize;
        let len = u8::try_from(len).map_err(|_| VoteStateViewError::InvalidVotesLength)?;
        let frame = Self { len };
        cursor.consume(frame.total_item_size());
        Ok(frame)
    }
}

#[repr(C)]
pub(super) struct LandedVoteItem {
    latency: u8,
    slot: [u8; 8],
    confirmation_count: [u8; 4],
}

impl ListFrame for LandedVotesListFrame {
    type Item = LockoutItem;

    #[cfg(test)]
    const ASSERT_ITEM_ALIGNMENT: () = {
        static_assertions::const_assert!(core::mem::align_of::<LockoutItem>() == 1);
    };

    fn len(&self) -> usize {
        self.len as usize
    }

    fn item_size(&self) -> usize {
        core::mem::size_of::<LandedVoteItem>()
    }

    unsafe fn read_item<'a>(&self, item_data: &'a [u8]) -> &'a Self::Item {
        &*(item_data[1..].as_ptr() as *const LockoutItem)
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
pub(super) struct AuthorizedVotersListFrame {
    pub(super) len: u8,
}

impl AuthorizedVotersListFrame {
    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) -> Result<Self> {
        let len = solana_serialize_utils::cursor::read_u64(cursor)
            .map_err(|_err| VoteStateViewError::AccountDataTooSmall)? as usize;
        let len =
            u8::try_from(len).map_err(|_| VoteStateViewError::InvalidAuthorizedVotersLength)?;
        let frame = Self { len };
        cursor.consume(frame.total_item_size());
        Ok(frame)
    }
}

#[repr(C)]
pub(super) struct AuthorizedVoterItem {
    epoch: [u8; 8],
    voter: Pubkey,
}

impl ListFrame for AuthorizedVotersListFrame {
    type Item = AuthorizedVoterItem;

    #[cfg(test)]
    const ASSERT_ITEM_ALIGNMENT: () = {
        static_assertions::const_assert!(core::mem::align_of::<AuthorizedVoterItem>() == 1);
    };

    fn len(&self) -> usize {
        self.len as usize
    }
}

impl<'a> ListView<'a, AuthorizedVotersListFrame> {
    pub(super) fn get_authorized_voter(self, epoch: Epoch) -> Option<&'a Pubkey> {
        for item in self.into_iter().rev() {
            let voter_epoch = u64::from_le_bytes(item.epoch);
            if voter_epoch <= epoch {
                return Some(&item.voter);
            }
        }

        None
    }
}

#[repr(C)]
pub struct EpochCreditsItem {
    epoch: [u8; 8],
    credits: [u8; 8],
    prev_credits: [u8; 8],
}

#[derive(Debug, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
pub(super) struct EpochCreditsListFrame {
    pub(super) len: u8,
}

impl EpochCreditsListFrame {
    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) -> Result<Self> {
        let len = solana_serialize_utils::cursor::read_u64(cursor)
            .map_err(|_err| VoteStateViewError::AccountDataTooSmall)? as usize;
        let len = u8::try_from(len).map_err(|_| VoteStateViewError::InvalidEpochCreditsLength)?;
        let frame = Self { len };
        cursor.consume(frame.total_item_size());
        Ok(frame)
    }
}

impl ListFrame for EpochCreditsListFrame {
    type Item = EpochCreditsItem;

    #[cfg(test)]
    const ASSERT_ITEM_ALIGNMENT: () = {
        static_assertions::const_assert!(core::mem::align_of::<EpochCreditsItem>() == 1);
    };

    fn len(&self) -> usize {
        self.len as usize
    }
}

impl EpochCreditsItem {
    #[inline]
    pub fn epoch(&self) -> u64 {
        u64::from_le_bytes(self.epoch)
    }
    #[inline]
    pub fn credits(&self) -> u64 {
        u64::from_le_bytes(self.credits)
    }
    #[inline]
    pub fn prev_credits(&self) -> u64 {
        u64::from_le_bytes(self.prev_credits)
    }
}

impl From<&EpochCreditsItem> for (Epoch, u64, u64) {
    fn from(item: &EpochCreditsItem) -> Self {
        (item.epoch(), item.credits(), item.prev_credits())
    }
}

pub(super) struct RootSlotView<'a> {
    frame: RootSlotFrame,
    buffer: &'a [u8],
}

impl<'a> RootSlotView<'a> {
    pub(super) fn new(frame: RootSlotFrame, buffer: &'a [u8]) -> Self {
        Self { frame, buffer }
    }
}

impl RootSlotView<'_> {
    pub(super) fn root_slot(&self) -> Option<Slot> {
        if !self.frame.has_root_slot {
            None
        } else {
            let root_slot = {
                let mut cursor = std::io::Cursor::new(self.buffer);
                cursor.consume(1);
                solana_serialize_utils::cursor::read_u64(&mut cursor).unwrap()
            };
            Some(root_slot)
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
pub(super) struct RootSlotFrame {
    pub(super) has_root_slot: bool,
}

impl RootSlotFrame {
    pub(super) fn total_size(&self) -> usize {
        1 + self.size()
    }

    pub(super) fn size(&self) -> usize {
        if self.has_root_slot {
            core::mem::size_of::<Slot>()
        } else {
            0
        }
    }

    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) -> Result<Self> {
        let byte = solana_serialize_utils::cursor::read_u8(cursor)
            .map_err(|_err| VoteStateViewError::AccountDataTooSmall)?;
        let has_root_slot = match byte {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(VoteStateViewError::InvalidRootSlotOption),
        }?;

        let frame = Self { has_root_slot };
        cursor.consume(frame.size());
        Ok(frame)
    }
}

pub(super) struct PriorVotersFrame;
impl PriorVotersFrame {
    pub(super) const fn total_size() -> usize {
        1545 // see test_prior_voters_total_size
    }

    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) {
        cursor.consume(PriorVotersFrame::total_size());
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_vote_interface::state::CircBuf};

    #[test]
    fn test_prior_voters_total_size() {
        #[repr(C)]
        pub(super) struct PriorVotersItem {
            voter: Pubkey,
            start_epoch_inclusive: [u8; 8],
            end_epoch_exclusive: [u8; 8],
        }

        let prior_voters_len = CircBuf::<()>::default().buf().len();
        let expected_total_size = prior_voters_len * core::mem::size_of::<PriorVotersItem>() +
            core::mem::size_of::<u64>() /* idx */ +
            core::mem::size_of::<bool>() /* is_empty */;
        assert_eq!(PriorVotersFrame::total_size(), expected_total_size);
    }
}
