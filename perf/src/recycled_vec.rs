use {
    crate::recycler::{RecyclerX, Reset},
    rand::{seq::SliceRandom, Rng},
    rayon::prelude::*,
    serde::{Deserialize, Serialize},
    std::{
        ops::{Deref, DerefMut, Index, IndexMut},
        slice::{Iter, SliceIndex},
        sync::Weak,
    },
};

// A vector wrapper which preallocates vector to be used
// with a recycler
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RecycledVec<T: Default + Clone + Sized> {
    x: Vec<T>,
    #[serde(skip)]
    recycler: Weak<RecyclerX<RecycledVec<T>>>,
}

impl<T: Default + Clone + Sized> Reset for RecycledVec<T> {
    fn reset(&mut self) {
        self.x.clear();
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
    pub fn preallocate(&mut self, size: usize) {
        let capacity_to_reserve = size.saturating_sub(self.x.capacity());
        self.x.reserve(capacity_to_reserve);
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

    pub fn push(&mut self, x: T) {
        self.x.push(x);
    }

    pub fn resize(&mut self, size: usize, elem: T) {
        self.x.resize(size, elem);
    }

    pub fn append(&mut self, other: &mut Vec<T>) {
        self.x.append(other);
    }

    pub fn shuffle<R: Rng>(&mut self, rng: &mut R) {
        self.x.shuffle(rng)
    }

    pub fn truncate(&mut self, len: usize) {
        self.x.truncate(len);
    }

    pub(crate) fn capacity(&self) -> usize {
        self.x.capacity()
    }
}

impl<T: Clone + Default + Sized> Clone for RecycledVec<T> {
    fn clone(&self) -> Self {
        let x = self.x.clone();
        debug!("clone PreallocatedVec: size: {}", self.x.capacity());
        Self {
            x,
            recycler: self.recycler.clone(),
        }
    }
}

impl<T: Sized + Default + Clone> Deref for RecycledVec<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.x
    }
}

impl<T: Sized + Default + Clone> DerefMut for RecycledVec<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.x
    }
}

impl<T: Sized + Default + Clone> Drop for RecycledVec<T> {
    fn drop(&mut self) {
        if let Some(recycler) = self.recycler.upgrade() {
            recycler.recycle(std::mem::take(self));
        }
    }
}

impl<T: Sized + Default + Clone + PartialEq> PartialEq for RecycledVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.x.eq(&other.x)
    }
}

impl<T: Sized + Default + Clone + PartialEq + Eq> Eq for RecycledVec<T> {}

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
