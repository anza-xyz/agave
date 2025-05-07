use super::field_frames::ListFrame;

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

    pub(super) fn len(&self) -> usize {
        self.frame.len()
    }

    pub(super) fn into_iter(self) -> ListViewIter<'a, F>
    where
        Self: Sized,
    {
        ListViewIter {
            index: 0,
            rev_index: 0,
            view: self,
        }
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
    index: usize,
    rev_index: usize,
    view: ListView<'a, F>,
}

impl<'a, F: ListFrame> Iterator for ListViewIter<'a, F>
where
    F::Item: 'a,
{
    type Item = &'a F::Item;
    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.view.len() {
            let item = self.view.item(self.index);
            self.index += 1;
            item
        } else {
            None
        }
    }
}

impl<'a, F: ListFrame> DoubleEndedIterator for ListViewIter<'a, F>
where
    F::Item: 'a,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.rev_index < self.view.len() {
            let item = self.view.item(self.view.len() - self.rev_index - 1);
            self.rev_index += 1;
            item
        } else {
            None
        }
    }
}
