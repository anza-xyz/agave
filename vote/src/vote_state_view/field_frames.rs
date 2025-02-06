use {
    super::{field_list_view::ListView, Result, VoteStateViewError},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_vote_interface::state::Lockout,
    std::io::BufRead,
};

pub(super) trait ListFrame {
    fn len(&self) -> usize;
    fn item_size(&self) -> usize;
    fn total_size(&self) -> usize {
        core::mem::size_of::<u64>() /* len */ + self.total_item_size()
    }
    fn total_item_size(&self) -> usize {
        self.len() * self.item_size()
    }
}

pub(super) struct VotesListFrame {
    len: usize,
    has_latency: bool,
}

impl VotesListFrame {
    pub(super) const fn new(len: usize, has_latency: bool) -> Self {
        Self { len, has_latency }
    }

    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>, has_latency: bool) -> Result<Self> {
        let len = solana_serialize_utils::cursor::read_u64(cursor)
            .map_err(|_err| VoteStateViewError::ParseError)? as usize;
        let frame = Self { len, has_latency };
        cursor.consume(frame.total_item_size());
        Ok(frame)
    }
}

impl ListFrame for VotesListFrame {
    fn len(&self) -> usize {
        self.len
    }

    fn item_size(&self) -> usize {
        core::mem::size_of::<u64>()
            + core::mem::size_of::<u32>()
            + if self.has_latency { 1 } else { 0 }
    }
}

impl<'a> ListView<'a, VotesListFrame> {
    pub(super) fn votes_iter(self) -> impl Iterator<Item = Lockout> + 'a {
        let has_latency = self.frame().has_latency;
        self.into_iter().map(move |item| {
            let mut cursor = std::io::Cursor::new(item);
            if has_latency {
                cursor.consume(1)
            }
            let slot = solana_serialize_utils::cursor::read_u64(&mut cursor).unwrap();
            let confirmation_count = solana_serialize_utils::cursor::read_u32(&mut cursor).unwrap();
            Lockout::new_with_confirmation_count(slot, confirmation_count)
        })
    }

    pub(super) fn last_lockout(&self) -> Option<Lockout> {
        if self.len() == 0 {
            return None;
        }

        let last_vote_data = self.last().unwrap();
        let mut cursor = std::io::Cursor::new(last_vote_data);
        if self.frame().has_latency {
            cursor.consume(1);
        }
        let slot = solana_serialize_utils::cursor::read_u64(&mut cursor).unwrap();
        let confirmation_count = solana_serialize_utils::cursor::read_u32(&mut cursor).unwrap();
        Some(Lockout::new_with_confirmation_count(
            slot,
            confirmation_count,
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
struct AuthorizedVoterItem {
    epoch: [u8; 8],
    voter: Pubkey,
}

impl ListFrame for AuthorizedVotersListFrame {
    fn len(&self) -> usize {
        self.len
    }

    fn item_size(&self) -> usize {
        core::mem::size_of::<AuthorizedVoterItem>()
    }
}

impl<'a> ListView<'a, AuthorizedVotersListFrame> {
    pub(super) fn get_authorized_voter(self, epoch: Epoch) -> Option<&'a Pubkey> {
        for item_data in self.into_iter().rev() {
            let item = unsafe { &*(item_data.as_ptr() as *const AuthorizedVoterItem) };
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

impl ListFrame for EpochCreditsListFrame {
    fn len(&self) -> usize {
        self.len
    }

    fn item_size(&self) -> usize {
        core::mem::size_of::<EpochCreditsItem>()
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

impl<'a> ListView<'a, EpochCreditsListFrame> {
    pub(super) fn epoch_credits_iter(self) -> impl Iterator<Item = &'a EpochCreditsItem> + 'a {
        self.into_iter()
            .map(|item_data| unsafe { &*(item_data.as_ptr() as *const EpochCreditsItem) })
    }

    pub(super) fn credits(self) -> u64 {
        self.last()
            .map(|item_data| unsafe { &*(item_data.as_ptr() as *const EpochCreditsItem) })
            .map(|item| item.credits())
            .unwrap_or(0)
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
            core::mem::size_of::<u64>()
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
impl PriorVotersFrame {
    pub(super) const fn total_size() -> usize {
        #[repr(C)]
        struct PriorVotersItem {
            pub voter: Pubkey,
            pub start_epoch_inclusive: Epoch,
            pub end_epoch_exclusive: Epoch,
        }

        const MAX_ITEMS: usize = 32;
        let prior_voter_item_size = core::mem::size_of::<PriorVotersItem>();
        let total_items_size = MAX_ITEMS * prior_voter_item_size;
        total_items_size + core::mem::size_of::<u64>() + core::mem::size_of::<bool>()
    }

    pub(super) fn read(cursor: &mut std::io::Cursor<&[u8]>) {
        cursor.consume(Self::total_size());
    }
}
