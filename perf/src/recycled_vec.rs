use {
    crate::recycler::{RecyclerX, Reset},
    rand::{seq::SliceRandom, Rng},
    rayon::prelude::*,
    serde::{Deserialize, Serialize},
    std::{
        ops::{Index, IndexMut},
        slice::{Iter, IterMut, SliceIndex},
        sync::Weak,
    },
};

// A vector wrapper where the underlying memory can be recycled.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RecycledVec<T: Default + Clone + Sized> {
    x: Vec<T>,
    #[serde(skip)]
    recycler: Weak<RecyclerX<RecycledVec<T>>>,
}

impl<T: Default + Clone + Sized> Reset for RecycledVec<T> {
    fn reset(&mut self) {
        self.resize(0, T::default());
    }
    fn warm(&mut self, size_hint: usize) {
        self.resize(size_hint, T::default());
    }
    fn set_recycler(&mut self, recycler: Weak<RecyclerX<Self>>) {
        self.recycler = recycler;
    }
}

impl<T: Clone + Default + Sized> From<RecycledVec<T>> for Vec<T> {
    fn from(mut recycled_vec: RecycledVec<T>) -> Self {
        recycled_vec.recycler = Weak::default();
        std::mem::take(&mut recycled_vec.x)
    }
}

impl<'a, T: Clone + Default + Sized> IntoIterator for &'a RecycledVec<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.x.iter()
    }
}

impl<T: Clone + Default + Sized, I: SliceIndex<[T]>> Index<I> for RecycledVec<T> {
    type Output = I::Output;

    #[inline]
    fn index(&self, index: I) -> &Self::Output {
        &self.x[index]
    }
}

impl<T: Clone + Default + Sized, I: SliceIndex<[T]>> IndexMut<I> for RecycledVec<T> {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        &mut self.x[index]
    }
}

impl<T: Clone + Default + Sized> RecycledVec<T> {
    pub fn iter(&self) -> Iter<'_, T> {
        self.x.iter()
    }

    pub fn iter_mut(&mut self) -> IterMut<'_, T> {
        self.x.iter_mut()
    }

    pub fn capacity(&self) -> usize {
        self.x.capacity()
    }
}

impl<'a, T: Clone + Send + Sync + Default + Sized> IntoParallelIterator for &'a RecycledVec<T> {
    type Iter = rayon::slice::Iter<'a, T>;
    type Item = &'a T;
    fn into_par_iter(self) -> Self::Iter {
        self.x.par_iter()
    }
}

impl<'a, T: Clone + Send + Sync + Default + Sized> IntoParallelIterator for &'a mut RecycledVec<T> {
    type Iter = rayon::slice::IterMut<'a, T>;
    type Item = &'a mut T;
    fn into_par_iter(self) -> Self::Iter {
        self.x.par_iter_mut()
    }
}

impl<T: Clone + Default + Sized> RecycledVec<T> {
    pub fn reserve(&mut self, size: usize) {
        self.x.reserve(size);
    }

    pub fn copy_from_slice(&mut self, data: &[T])
    where
        T: Copy,
    {
        self.x.copy_from_slice(data);
    }

    pub fn from_vec(source: Vec<T>) -> Self {
        Self {
            x: source,
            recycler: Weak::default(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::from_vec(Vec::with_capacity(capacity))
    }

    pub fn is_empty(&self) -> bool {
        self.x.is_empty()
    }

    pub fn len(&self) -> usize {
        self.x.len()
    }

    pub fn as_ptr(&self) -> *const T {
        self.x.as_ptr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.x.as_mut_ptr()
    }

    pub fn push(&mut self, x: T) {
        self.x.push(x);
    }

    pub fn truncate(&mut self, size: usize) {
        self.x.truncate(size);
    }

    pub fn resize(&mut self, size: usize, elem: T) {
        self.x.resize(size, elem);
    }

    pub fn append(&mut self, other: &mut Vec<T>) {
        self.x.append(other);
    }

    /// Forces the length of the vector to `new_len`.
    ///
    /// This is a low-level operation that maintains none of the normal
    /// invariants of the type. Normally changing the length of a vector
    /// is done using one of the safe operations instead, such as
    /// [`truncate`], [`resize`], [`extend`], or [`clear`].
    ///
    /// [`truncate`]: Vec::truncate
    /// [`resize`]: Vec::resize
    /// [`extend`]: Extend::extend
    /// [`clear`]: Vec::clear
    ///
    /// # Safety
    ///
    /// - `new_len` must be less than or equal to [`capacity()`].
    /// - The elements at `old_len..new_len` must be initialized.
    ///
    /// [`capacity()`]: Vec::capacity
    ///
    pub unsafe fn set_len(&mut self, size: usize) {
        self.x.set_len(size);
    }

    pub fn shuffle<R: Rng>(&mut self, rng: &mut R) {
        self.x.shuffle(rng)
    }
}

impl<T: Clone + Default + Sized> Clone for RecycledVec<T> {
    fn clone(&self) -> Self {
        let x = self.x.clone();
        debug!("clone RecycledVec: size: {}", self.x.capacity(),);
        Self {
            x,
            recycler: self.recycler.clone(),
        }
    }
}

impl<T: Sized + Default + Clone> Drop for RecycledVec<T> {
    fn drop(&mut self) {
        if let Some(recycler) = self.recycler.upgrade() {
            recycler.recycle(std::mem::take(self));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recycled_vec() {
        let mut mem = RecycledVec::with_capacity(10);
        mem.push(50);
        mem.resize(2, 10);
        assert_eq!(mem[0], 50);
        assert_eq!(mem[1], 10);
        assert_eq!(mem.len(), 2);
        assert!(!mem.is_empty());
        let mut iter = mem.iter();
        assert_eq!(*iter.next().unwrap(), 50);
        assert_eq!(*iter.next().unwrap(), 10);
        assert_eq!(iter.next(), None);
    }
}
