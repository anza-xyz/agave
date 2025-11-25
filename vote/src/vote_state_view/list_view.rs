use {super::field_frames::ListFrame, std::slice::ChunksExact};

pub(super) struct ListView<'a, F> {
    frame: F,
    item_buffer: &'a [u8],
}

impl<'a, F: ListFrame> ListView<'a, F> {
    pub(super) fn new(frame: F, buffer: &'a [u8]) -> Self {
        let len_offset = core::mem::size_of::<u64>();
        let item_buffer = &buffer[len_offset..];
        Self { frame, item_buffer }
    }

    #[inline]
    pub(super) fn len(&self) -> usize {
        self.frame.len()
    }

    pub(super) fn last(&self) -> Option<&F::Item> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        self.item(len - 1)
    }

    fn item(&self, index: usize) -> Option<&'a F::Item> {
        if index >= self.len() {
            return None;
        }

        let offset = index * self.frame.item_size();
        // SAFETY: `item_buffer` is long enough to contain all items
        let item_data = &self.item_buffer[offset..offset + self.frame.item_size()];
        // SAFETY: `item_data` is long enough to contain an item
        Some(unsafe { self.frame.read_item(item_data) })
    }
}

pub(super) struct ListViewIter<'a, F> {
    buffer: ChunksExact<'a, u8>,
    frame: F,
}

impl<'a, F: ListFrame> Iterator for ListViewIter<'a, F>
where
    F::Item: 'a,
{
    type Item = &'a F::Item;

    fn next(&mut self) -> Option<Self::Item> {
        let item_data = self.buffer.next()?;
        // SAFETY: `item_data` is chunked by `self.frame.item_size()`
        Some(unsafe { self.frame.read_item(item_data) })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.buffer.size_hint()
    }

    fn count(self) -> usize {
        self.buffer.count()
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        let item_data = self.buffer.nth(n)?;
        // SAFETY: `item_data` is chunked by `self.frame.item_size()`
        Some(unsafe { self.frame.read_item(item_data) })
    }

    fn last(mut self) -> Option<Self::Item> {
        self.next_back()
    }
}

impl<'a, F: ListFrame> DoubleEndedIterator for ListViewIter<'a, F>
where
    F::Item: 'a,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        let item_data = self.buffer.next_back()?;
        // SAFETY: `item_data` is chunked by `self.frame.item_size()`
        Some(unsafe { self.frame.read_item(item_data) })
    }
}

impl<'a, F: ListFrame> IntoIterator for ListView<'a, F>
where
    F::Item: 'a,
{
    type Item = &'a F::Item;
    type IntoIter = ListViewIter<'a, F>;

    fn into_iter(self) -> Self::IntoIter {
        let item_size = self.frame.item_size();
        let total_bytes = self.frame.len() * item_size;
        let slice = &self.item_buffer[..total_bytes];
        ListViewIter {
            buffer: slice.chunks_exact(item_size),
            frame: self.frame,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::field_frames::{LandedVotesListFrame, LockoutListFrame},
        *,
    };

    fn build_lockout_buffer(len: usize, make: impl Fn(usize) -> (u64, u32)) -> Vec<u8> {
        let mut buf = Vec::with_capacity(size_of::<u64>() + len * 12);
        buf.extend_from_slice(&(len as u64).to_le_bytes());
        for i in 0..len {
            let (slot, count) = make(i);
            buf.extend_from_slice(&slot.to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
        }
        buf
    }

    fn build_landed_buffer(len: usize, make: impl Fn(usize) -> (u8, u64, u32)) -> Vec<u8> {
        let mut buf = Vec::with_capacity(size_of::<u64>() + len * 13);
        buf.extend_from_slice(&(len as u64).to_le_bytes());
        for i in 0..len {
            let (latency, slot, count) = make(i);
            buf.push(latency);
            buf.extend_from_slice(&slot.to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
        }
        buf
    }

    #[test]
    fn iter_lockout_forward_and_rev() {
        let len = 5usize;
        let buffer = build_lockout_buffer(len, |i| (i as u64, (10 + i) as u32));
        let frame = LockoutListFrame { len: len as u8 };
        let view = ListView::new(frame, &buffer);

        let forward: Vec<(u64, u32)> = view
            .into_iter()
            .map(|it| (it.slot(), it.confirmation_count()))
            .collect();
        assert_eq!(
            forward,
            (0..len)
                .map(|i| (i as u64, (10 + i) as u32))
                .collect::<Vec<_>>()
        );

        let view = ListView::new(frame, &buffer);
        let rev: Vec<(u64, u32)> = view
            .into_iter()
            .rev()
            .map(|it| (it.slot(), it.confirmation_count()))
            .collect();
        assert_eq!(
            rev,
            (0..len)
                .rev()
                .map(|i| (i as u64, (10 + i) as u32))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn iter_lockout_mixed_front_back() {
        let len = 5usize;
        let buffer = build_lockout_buffer(len, |i| (i as u64, (100 + i) as u32));
        let frame = LockoutListFrame { len: len as u8 };
        let mut it = ListView::new(frame, &buffer).into_iter();

        let out = [
            (it.next().map(|x| (x.slot(), x.confirmation_count()))).unwrap(), // 0
            (it.next_back().map(|x| (x.slot(), x.confirmation_count()))).unwrap(), // 4
            (it.next().map(|x| (x.slot(), x.confirmation_count()))).unwrap(), // 1
            (it.next_back().map(|x| (x.slot(), x.confirmation_count()))).unwrap(), // 3
            (it.next().map(|x| (x.slot(), x.confirmation_count()))).unwrap(), // 2
        ];
        assert!(it.next().is_none());

        let expected = [
            (0u64, 100u32),
            (4u64, 104u32),
            (1u64, 101u32),
            (3u64, 103u32),
            (2u64, 102u32),
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn iter_landed_stride_and_rev() {
        let len = 4usize;
        let buffer =
            build_landed_buffer(len, |i| ((200 + i) as u8, (10 + i) as u64, (20 + i) as u32));
        let frame = LandedVotesListFrame { len: len as u8 };

        // Forward
        let view = ListView::new(frame, &buffer);
        let fwd: Vec<(u64, u32)> = view
            .into_iter()
            .map(|it| (it.slot(), it.confirmation_count()))
            .collect();
        assert_eq!(
            fwd,
            (0..len)
                .map(|i| ((10 + i) as u64, (20 + i) as u32))
                .collect::<Vec<_>>()
        );

        // Reverse
        let view = ListView::new(frame, &buffer);
        let rev: Vec<(u64, u32)> = view
            .into_iter()
            .rev()
            .map(|it| (it.slot(), it.confirmation_count()))
            .collect();
        assert_eq!(
            rev,
            (0..len)
                .rev()
                .map(|i| ((10 + i) as u64, (20 + i) as u32))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn size_hint_decreases() {
        let len = 3usize;
        let buffer = build_lockout_buffer(len, |i| (i as u64, i as u32));
        let frame = LockoutListFrame { len: len as u8 };
        let mut it = ListView::new(frame, &buffer).into_iter();

        assert_eq!(it.size_hint(), (3, Some(3)));
        assert!(it.next().is_some());
        assert_eq!(it.size_hint(), (2, Some(2)));
        assert!(it.next_back().is_some());
        assert_eq!(it.size_hint(), (1, Some(1)));
        assert!(it.next().is_some());
        assert_eq!(it.size_hint(), (0, Some(0)));
        assert!(it.next().is_none());
        assert!(it.next_back().is_none());
    }
}
