use slab::Slab;

pub(crate) struct FixedSlab<T> {
    inner: Slab<T>,
}

impl<T> FixedSlab<T> {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: Slab::with_capacity(cap),
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    pub fn insert(&mut self, value: T) -> usize {
        if self.is_full() {
            panic!("FixedSlab is full, cannot insert new value");
        }
        self.inner.insert(value)
    }

    pub fn remove(&mut self, key: usize) -> T {
        self.inner.remove(key)
    }

    pub fn get_mut(&mut self, key: usize) -> Option<&mut T> {
        self.inner.get_mut(key)
    }
}
