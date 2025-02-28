use {
    super::{list_view::ListView, Result, VoteStateViewError},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_vote_interface::state::Lockout,
    std::io::BufRead,
};

pub(super) trait ListFrame<'frame> {
    type Item: 'frame;

    fn len(&self) -> usize;
    fn item_size(&self) -> usize {
        core::mem::size_of::<Self::Item>()
    }
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

impl ListFrame<'_> for VotesFrame {
    type Item = LockoutItem;

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

pub(super) struct LockoutListFrame {
    len: usize,
}

impl LockoutListFrame {
    pub(super) const fn new(len: usize) -> Self {
        Self { len }
    }

    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) -> Result<Self> {
        let len = solana_serialize_utils::cursor::read_u64(cursor)
            .map_err(|_err| VoteStateViewError::ParseError)? as usize;
        let frame = Self { len };
        cursor.consume(frame.total_item_size());
        Ok(frame)
    }
}

impl ListFrame<'_> for LockoutListFrame {
    type Item = LockoutItem;

    fn len(&self) -> usize {
        self.len
    }
}

pub(super) struct LandedVotesListFrame {
    len: usize,
}

impl LandedVotesListFrame {
    pub(super) const fn new(len: usize) -> Self {
        Self { len }
    }

    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) -> Result<Self> {
        let len = solana_serialize_utils::cursor::read_u64(cursor)
            .map_err(|_err| VoteStateViewError::ParseError)? as usize;
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

impl ListFrame<'_> for LandedVotesListFrame {
    type Item = LockoutItem;

    fn len(&self) -> usize {
        self.len
    }

    fn item_size(&self) -> usize {
        core::mem::size_of::<LandedVoteItem>()
    }

    unsafe fn read_item<'a>(&self, item_data: &'a [u8]) -> &'a Self::Item {
        &*(item_data[1..].as_ptr() as *const LockoutItem)
    }
}

impl<'a, T: ListFrame<'a, Item = LockoutItem>> ListView<'a, T> {
    pub(super) fn last_lockout(&self) -> Option<Lockout> {
        if self.len() == 0 {
            return None;
        }

        let last_item = self.last().unwrap();
        Some(Lockout::new_with_confirmation_count(
            last_item.slot(),
            last_item.confirmation_count(),
        ))
    }
}

pub(super) struct AuthorizedVotersListFrame {
    len: usize,
}

impl AuthorizedVotersListFrame {
    pub(super) const fn new(len: usize) -> Self {
        Self { len }
    }

    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) -> Result<Self> {
        let len = solana_serialize_utils::cursor::read_u64(cursor)
            .map_err(|_err| VoteStateViewError::ParseError)? as usize;
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

impl ListFrame<'_> for AuthorizedVotersListFrame {
    type Item = AuthorizedVoterItem;

    fn len(&self) -> usize {
        self.len
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

pub(super) struct EpochCreditsListFrame {
    len: usize,
}

impl EpochCreditsListFrame {
    pub(super) const fn new(len: usize) -> Self {
        Self { len }
    }

    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) -> Result<Self> {
        let len = solana_serialize_utils::cursor::read_u64(cursor)
            .map_err(|_err| VoteStateViewError::ParseError)? as usize;
        let frame = Self { len };
        cursor.consume(frame.total_item_size());
        Ok(frame)
    }
}

impl ListFrame<'_> for EpochCreditsListFrame {
    type Item = EpochCreditsItem;

    fn len(&self) -> usize {
        self.len
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

impl ListView<'_, EpochCreditsListFrame> {
    pub(super) fn credits(self) -> u64 {
        self.last().map(|item| item.credits()).unwrap_or(0)
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

pub(super) struct RootSlotFrame {
    has_root_slot: bool,
}

impl RootSlotFrame {
    pub(super) const fn new(has_root_slot: bool) -> Self {
        Self { has_root_slot }
    }

    pub(super) fn has_root_slot(&self) -> bool {
        self.has_root_slot
    }

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
        let has_root_slot = solana_serialize_utils::cursor::read_bool(cursor)
            .map_err(|_err| VoteStateViewError::ParseError)?;
        let frame = Self { has_root_slot };
        cursor.consume(frame.size());
        Ok(frame)
    }
}

pub(super) struct PriorVotersFrame;

impl ListFrame<'_> for PriorVotersFrame {
    type Item = PriorVotersItem;

    fn len(&self) -> usize {
        const MAX_ITEMS: usize = 32;
        MAX_ITEMS
    }

    fn total_size(&self) -> usize {
        self.total_item_size() +
            core::mem::size_of::<u64>() /* idx */ +
            core::mem::size_of::<bool>() /* is_empty */
    }
}

#[repr(C)]
pub(super) struct PriorVotersItem {
    voter: Pubkey,
    start_epoch_inclusive: Epoch,
    end_epoch_exclusive: Epoch,
}

impl PriorVotersFrame {
    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) {
        cursor.consume(PriorVotersFrame.total_size());
    }
}
