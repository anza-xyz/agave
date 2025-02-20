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

    pub(super) fn frame(&self) -> &F {
        &self.frame
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

    pub(super) fn last(&self) -> Option<&'a [u8]> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        Some(self.item(len - 1))
    }

    fn item(&self, index: usize) -> &'a [u8] {
        let offset = index * self.frame.item_size();
        &self.item_buffer[offset..offset + self.frame.item_size()]
    }
}

pub(super) struct ListViewIter<'a, F> {
    index: usize,
    rev_index: usize,
    view: ListView<'a, F>,
}

impl<'a, F: ListFrame> Iterator for ListViewIter<'a, F> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.view.len() {
            let item = self.view.item(self.index);
            self.index += 1;
            Some(item)
        } else {
            None
        }
    }
}

impl<F: ListFrame> DoubleEndedIterator for ListViewIter<'_, F> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.rev_index < self.view.len() {
            let item = self.view.item(self.view.len() - self.rev_index - 1);
            self.rev_index += 1;
            Some(item)
        } else {
            None
        }
    }
}
