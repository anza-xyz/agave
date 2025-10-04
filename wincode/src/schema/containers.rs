//! This module provides specialized "container" types that can be used to opt
//! into optimized read/write implementations or specialized length encodings.
//!
//! # Examples
//! Raw byte vec with default bincode length encoding:
//!
//! ```
//! use wincode::{containers::{self, Pod}, compound};
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Serialize, Deserialize, Debug)]
//! struct MyStruct {
//!     vec: Vec<u8>,
//! }
//!
//! compound! {
//!     MyStruct {
//!         vec: containers::Vec<Pod<u8>>,
//!     }
//! }
//!
//! let my_struct = MyStruct {
//!     vec: vec![1, 2, 3],
//! };
//! let wincode_bytes = wincode::serialize(&my_struct).unwrap();
//! let bincode_bytes = bincode::serialize(&my_struct).unwrap();
//! assert_eq!(wincode_bytes, bincode_bytes);
//! ```
//!
//! Raw byte vec with solana short vec length encoding:
//!
//! ```
//! use wincode::{containers::{self, Pod}, compound, len::ShortU16Len};
//! use serde::{Serialize, Deserialize};
//! use solana_short_vec;
//!
//! #[derive(Serialize, Deserialize)]
//! struct MyStruct {
//!     #[serde(with = "solana_short_vec")]
//!     vec: Vec<u8>,
//! }
//!
//! compound! {
//!     MyStruct {
//!         vec: containers::Vec<Pod<u8>, ShortU16Len>,
//!     }
//! }
//!
//! let my_struct = MyStruct {
//!     vec: vec![1, 2, 3],
//! };
//! let wincode_bytes = wincode::serialize(&my_struct).unwrap();
//! let bincode_bytes = bincode::serialize(&my_struct).unwrap();
//! assert_eq!(wincode_bytes, bincode_bytes);
//! ```
//!
//! Vector with non-POD elements and custom length encoding:
//!
//! ```
//! use wincode::{containers::{self, Elem}, compound, len::ShortU16Len};
//! use serde::{Serialize, Deserialize};
//! use solana_short_vec;
//!
//! #[derive(Serialize, Deserialize)]
//! struct Point {
//!     x: u64,
//!     y: u64,
//! }
//!
//! compound! {
//!     Point {
//!         x: u64,
//!         y: u64,
//!     }
//! }
//!
//! #[derive(Serialize, Deserialize)]
//! struct MyStruct {
//!     #[serde(with = "solana_short_vec")]
//!     vec: Vec<Point>,
//! }
//!
//! compound! {
//!     MyStruct {
//!         vec: containers::Vec<Elem<Point>, ShortU16Len>,
//!     }
//! }
//!
//! let my_struct = MyStruct {
//!     vec: vec![Point { x: 1, y: 2 }, Point { x: 3, y: 4 }],
//! };
//! let wincode_bytes = wincode::serialize(&my_struct).unwrap();
//! let bincode_bytes = bincode::serialize(&my_struct).unwrap();
//! assert_eq!(wincode_bytes, bincode_bytes);
//! ```
use {
    crate::{
        error::Result,
        io::{Reader, Writer},
        schema::{SchemaRead, SchemaWrite},
    },
    core::{marker::PhantomData, mem::MaybeUninit},
};
#[cfg(feature = "alloc")]
use {
    crate::{
        len::{BincodeLen, SeqLen},
        schema::{size_of_elem_iter, write_elem_iter},
    },
    core::slice,
};

/// A [`Vec`](std::vec::Vec) with a customizable length encoding and optimized
/// read/write implementation for [`Pod`].
#[cfg(feature = "alloc")]
pub struct Vec<T, Len = BincodeLen>(PhantomData<Len>, PhantomData<T>);

/// A [`VecDeque`](std::collections::VecDeque) with a customizable length encoding and optimized
/// read/write implementation for [`Pod`].
#[cfg(feature = "alloc")]
pub struct VecDeque<T, Len = BincodeLen>(PhantomData<Len>, PhantomData<T>);

/// A [`Box<[T]>`](std::boxed::Box) with a customizable length encoding and optimized
/// read/write implementation for [`Pod`].
///
/// # Examples
///
/// ```
/// use wincode::{containers::{self, BoxedSlice, Pod}, compound};
/// use serde::{Serialize, Deserialize};
/// # use std::array;
///
/// #[derive(Serialize, Deserialize, Clone, Copy)]
/// #[repr(transparent)]
/// struct Address([u8; 32]);
///
/// #[derive(Serialize, Deserialize)]
/// struct MyStruct {
///     address: Box<[Address]>
/// }
///
/// compound! {
///     MyStruct {
///         address: BoxedSlice<Pod<Address>>,
///     }
/// }
///
/// let my_struct = MyStruct {
///     address: vec![Address(array::from_fn(|i| i as u8)); 10].into_boxed_slice(),
/// };
/// let wincode_bytes = wincode::serialize(&my_struct).unwrap();
/// let bincode_bytes = bincode::serialize(&my_struct).unwrap();
/// assert_eq!(wincode_bytes, bincode_bytes);
/// ```
#[cfg(feature = "alloc")]
pub struct BoxedSlice<T, Len = BincodeLen>(PhantomData<T>, PhantomData<Len>);

/// Indicates that the type is an element of a sequence, composable with [`containers`](self).
///
/// Prefer [`Pod`] for types representable as raw bytes.
pub struct Elem<T>(PhantomData<T>);

/// Indicates that the type is represented by raw bytes, composable with sequence [`containers`](self)
/// or compound types (structs, tuples) for an optimized read/write implementation.
///
/// Use [`Elem`] with [`containers`](self) that aren't comprised of POD.
///
/// This can be useful outside of sequences as well, for example on newtype structs
/// containing byte arrays / vectors with `#[repr(transparent)]`.
///
/// # Examples
///
/// A repr-transparent newtype struct with a byte array:
/// ```
/// # use wincode::{containers::{self, Pod}, compound};
/// # use serde::{Serialize, Deserialize};
/// # use std::array;
/// #[derive(Serialize, Deserialize)]
/// #[repr(transparent)]
/// struct Address([u8; 32]);
///
/// #[derive(Serialize, Deserialize)]
/// struct MyStruct {
///     address: Address
/// }
///
/// compound! {
///     MyStruct {
///         address: Pod<Address>,
///     }
/// }
///
/// let my_struct = MyStruct {
///     address: Address(array::from_fn(|i| i as u8)),
/// };
/// let wincode_bytes = wincode::serialize(&my_struct).unwrap();
/// let bincode_bytes = bincode::serialize(&my_struct).unwrap();
/// assert_eq!(wincode_bytes, bincode_bytes);
/// ```
pub struct Pod<T>(PhantomData<T>);

impl<T> SchemaWrite for Pod<T> {
    type Src = T;

    #[inline]
    fn size_of(_src: &Self::Src) -> Result<usize> {
        Ok(size_of::<T>())
    }

    #[inline]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        // SAFETY: `T` is plain ol' data.
        unsafe { writer.write_t(src) }
    }
}

impl<T> SchemaRead for Pod<T> {
    type Dst = T;

    fn read(reader: &mut Reader, dst: &mut MaybeUninit<Self::Dst>) -> Result<()> {
        // SAFETY: `T` is plain ol' data.
        unsafe { reader.read_t(dst) }
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaWrite for Vec<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = alloc::vec::Vec<T::Src>;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> Result<usize> {
        size_of_elem_iter::<T, Len>(src.iter())
    }

    #[inline(always)]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        write_elem_iter::<T, Len>(writer, src.iter())
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaRead for Vec<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaRead,
{
    type Dst = alloc::vec::Vec<T::Dst>;

    /// Read a sequence of `T::Dst`s from `reader` into `dst`.
    ///
    /// This provides a `*mut T::Dst` for each slot in the allocated Vec
    /// to facilitate in-place writing of Vec memory.
    ///
    /// Prefer [`Vec<Pod<T>, Len>`] for sequences representable as raw bytes.
    ///
    /// # Safety
    ///
    /// - `T::read` must properly initialize elements.
    fn read(reader: &mut Reader, dst: &mut MaybeUninit<Self::Dst>) -> Result<()> {
        let len = Len::size_hint_cautious::<T::Dst>(reader)?;
        let mut vec = alloc::vec::Vec::with_capacity(len);
        // Get a raw pointer to the Vec memory to facilitate in-place writing.
        let vec_ptr = vec.spare_capacity_mut();
        #[allow(clippy::needless_range_loop)]
        for i in 0..len {
            // Yield the current slot to the caller.
            if let Err(e) = T::read(reader, &mut vec_ptr[i]) {
                // SAFETY: we've read at least `i` elements.
                unsafe { vec.set_len(i) };
                return Err(e);
            }
        }
        // SAFETY: Caller ensures `T::read` properly initializes elements.
        unsafe { vec.set_len(len) }
        dst.write(vec);
        Ok(())
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaWrite for Vec<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Src = alloc::vec::Vec<T>;

    #[inline]
    fn size_of(src: &Self::Src) -> Result<usize> {
        #[allow(clippy::arithmetic_side_effects)]
        Ok(Len::bytes_needed(src.len())? + size_of_val(src.as_slice()))
    }

    #[inline]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        Len::encode_len(writer, src.len())?;
        // SAFETY: Caller ensures `src` is plain ol' data.
        unsafe { writer.write_slice_t(src.as_slice()) }
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaRead for Vec<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Dst = alloc::vec::Vec<T>;

    /// Read a sequence of bytes or a sequence of fixed length byte arrays from the cursor into `dst`.
    ///
    /// This reads the entire sequence at once, rather than yielding each element to the caller.
    ///
    /// Should be used with types representable by raw bytes, like `Vec<u8>` or `Vec<[u8; N]>`.
    ///
    /// # Safety
    ///
    /// - `T` must be plain ol' data, valid for writes of `size_of::<T>()` bytes.
    fn read(reader: &mut Reader, dst: &mut MaybeUninit<Self::Dst>) -> Result<()> {
        let len = Len::size_hint_cautious::<T>(reader)?;
        let mut vec = alloc::vec::Vec::with_capacity(len);
        let spare_capacity = vec.spare_capacity_mut();
        let slice = unsafe {
            slice::from_raw_parts_mut(
                spare_capacity.as_mut_ptr().cast(),
                size_of_val(spare_capacity),
            )
        };
        reader.read_exact(slice)?;
        // SAFETY: Caller ensures `T` is plain ol' data and can be initialized by raw byte reads.
        unsafe { vec.set_len(len) }
        dst.write(vec);
        Ok(())
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaWrite for BoxedSlice<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Src = alloc::boxed::Box<[T]>;

    #[inline]
    fn size_of(src: &Self::Src) -> Result<usize> {
        #[allow(clippy::arithmetic_side_effects)]
        Ok(Len::bytes_needed(src.len())? + size_of_val(&src[..]))
    }

    #[inline]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        Len::encode_len(writer, src.len())?;
        // SAFETY: Caller ensures `T` is plain ol' data.
        unsafe { writer.write_slice_t(&src[..]) }
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaRead for BoxedSlice<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Dst = alloc::boxed::Box<[T]>;

    #[inline(always)]
    fn read(reader: &mut Reader, dst: &mut MaybeUninit<Self::Dst>) -> Result<()> {
        let mut vec = MaybeUninit::uninit();
        // Leverage drop safety of the Vec impl.
        <Vec<Pod<T>, Len>>::read(reader, &mut vec)?;

        // SAFETY: The given `SchemaRead` must properly initialize the dst.
        unsafe { dst.write(vec.assume_init().into_boxed_slice()) };
        Ok(())
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaWrite for BoxedSlice<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = alloc::boxed::Box<[T::Src]>;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> Result<usize> {
        size_of_elem_iter::<T, Len>(src.iter())
    }

    #[inline(always)]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        write_elem_iter::<T, Len>(writer, src.iter())
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaRead for BoxedSlice<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaRead,
{
    type Dst = alloc::boxed::Box<[T::Dst]>;

    #[inline(always)]
    fn read(reader: &mut Reader, dst: &mut MaybeUninit<Self::Dst>) -> Result<()> {
        let mut v = MaybeUninit::uninit();
        <Vec<Elem<T>, Len>>::read(reader, &mut v)?;
        // SAFETY: The given `SchemaRead` must properly initialize the dst.
        unsafe { dst.write(v.assume_init().into_boxed_slice()) };
        Ok(())
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaWrite for VecDeque<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Src = alloc::collections::VecDeque<T>;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> Result<usize> {
        #[allow(clippy::arithmetic_side_effects)]
        Ok(Len::bytes_needed(src.len())? + size_of::<T>() * src.len())
    }

    #[inline(always)]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        Len::encode_len(writer, src.len())?;
        let (front, back) = src.as_slices();
        unsafe {
            // SAFETY:
            // - Caller ensures given `T` is plain ol' data.
            // - `front` and `back` are valid non-overlapping slices.
            writer.write_slice_t(front)?;
            writer.write_slice_t(back)?;
        }
        Ok(())
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaRead for VecDeque<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Dst = alloc::collections::VecDeque<T>;

    #[inline(always)]
    fn read(reader: &mut Reader, dst: &mut MaybeUninit<Self::Dst>) -> Result<()> {
        let mut vec = MaybeUninit::uninit();
        // Leverage the contiguous read optimization of `Vec`.
        // From<Vec<T>> for VecDeque<T> is basically free.
        <Vec<Pod<T>, Len>>::read(reader, &mut vec)?;

        // SAFETY: The given `SchemaRead` must properly initialize the dst.
        unsafe { dst.write(vec.assume_init().into()) };
        Ok(())
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaWrite for VecDeque<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = alloc::collections::VecDeque<T::Src>;

    #[inline(always)]
    fn size_of(value: &Self::Src) -> Result<usize> {
        size_of_elem_iter::<T, Len>(value.iter())
    }

    #[inline(always)]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        write_elem_iter::<T, Len>(writer, src.iter())
    }
}

#[cfg(feature = "alloc")]
impl<T, Len> SchemaRead for VecDeque<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaRead,
{
    type Dst = alloc::collections::VecDeque<T::Dst>;

    #[inline(always)]
    fn read(reader: &mut Reader, dst: &mut MaybeUninit<Self::Dst>) -> Result<()> {
        let mut vec = MaybeUninit::uninit();
        // Leverage the contiguous read optimization of `Vec`.
        // From<Vec<T>> for VecDeque<T> is basically free.
        <Vec<Elem<T>, Len>>::read(reader, &mut vec)?;

        // SAFETY: The given `SchemaRead` must properly initialize the dst.
        unsafe { dst.write(vec.assume_init().into()) };

        Ok(())
    }
}
