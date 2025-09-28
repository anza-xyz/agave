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
        len::{BincodeLen, SeqLen},
        schema::{size_of_elem_iter, write_elem_iter, SchemaRead, SchemaWrite},
    },
    std::{marker::PhantomData, mem::MaybeUninit, ptr},
};

/// A [`Vec`](std::vec::Vec) with a customizable length encoding and optimized
/// read/write implementation for [`Pod`].
pub struct Vec<T, Len = BincodeLen>(PhantomData<Len>, PhantomData<T>);

/// A [`VecDeque`](std::collections::VecDeque) with a customizable length encoding and optimized
/// read/write implementation for [`Pod`].
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

    #[inline(always)]
    fn size_of(_src: &Self::Src) -> Result<usize> {
        Ok(size_of::<T>())
    }

    #[inline(always)]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        // SAFETY: caller ensures `T` is plain ol' data.
        unsafe { writer.write_t(src) }
    }
}

impl<T> SchemaRead for Pod<T> {
    type Dst = T;

    /// Read into `dst` from `reader`.
    ///
    /// # Safety
    ///
    /// - `dst` must be a valid pointer to a `T`.
    /// - `T` must be plain ol' data.
    #[inline(always)]
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        // SAFETY:
        // - Caller ensures `dst` is a valid pointer to a T.
        // - Caller ensures `T` is plain ol' data.
        unsafe { reader.read_t(dst) }
    }
}

impl<T, Len> SchemaWrite for Vec<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = std::vec::Vec<T::Src>;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> Result<usize> {
        size_of_elem_iter::<T, Len>(src.iter())
    }

    #[inline(always)]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        write_elem_iter::<T, Len>(writer, src.iter())
    }
}

impl<T, Len> SchemaRead for Vec<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaRead,
{
    type Dst = std::vec::Vec<T::Dst>;

    /// Read a sequence of `T::Dst`s from `reader` into `dst`.
    ///
    /// This provides a `*mut T::Dst` for each slot in the allocated Vec
    /// to facilitate in-place writing of Vec memory.
    ///
    /// Prefer [`Vec<Pod<T>, Len>`] for sequences representable as raw bytes.
    ///
    /// # Safety
    ///
    /// - `dst` must not overlap with the cursor.
    /// - `dst` must be valid pointer to a `Vec<T::Dst>`.
    /// - `T::read` must properly initialize elements.
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        let len = Len::size_hint_cautious::<T::Dst>(reader)?;
        let mut vec = std::vec::Vec::with_capacity(len);
        // Get a raw pointer to the Vec memory to facilitate in-place writing.
        let vec_ptr = vec.spare_capacity_mut().as_mut_ptr();
        for i in 0..len {
            // Yield the current slot to the caller.
            if let Err(e) = T::read(reader, vec_ptr.add(i) as *mut T::Dst) {
                unsafe {
                    // SAFETY: we've read at least `i` elements.
                    vec.set_len(i);
                }
                return Err(e);
            }
        }
        unsafe {
            // SAFETY: Caller ensures `parse_t` properly initializes elements.
            vec.set_len(len);
            // SAFETY: Caller ensures `ptr` is a valid ptr to a Vec<T>.
            ptr::write(dst, vec);
        }
        Ok(())
    }
}

impl<T, Len> SchemaWrite for Vec<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Src = std::vec::Vec<T>;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> Result<usize> {
        #[allow(clippy::arithmetic_side_effects)]
        Ok(Len::bytes_needed(src.len())? + size_of_val(src.as_slice()))
    }

    #[inline(always)]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        Len::encode_len(writer, src.len())?;
        // SAFETY: Caller ensures `src` is plain ol' data.
        unsafe { writer.write_exact(src.as_ptr() as *const u8, size_of_val(src.as_slice())) }
    }
}

impl<T, Len> SchemaRead for Vec<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Dst = std::vec::Vec<T>;

    /// Read a sequence of bytes or a sequence of fixed length byte arrays from the cursor into `dst`.
    ///
    /// This reads the entire sequence at once, rather than yielding each element to the caller.
    ///
    /// Should be used with types representable by raw bytes, like `Vec<u8>` or `Vec<[u8; N]>`.
    ///
    /// # Safety
    ///
    /// - `dst` must not overlap with the cursor.
    /// - `dst` must be valid pointer to a `Vec<T>`.
    /// - `T` must be plain ol' data, valid for writes of `size_of::<T>()` bytes.
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        let len = Len::size_hint_cautious::<T>(reader)?;
        let mut vec = std::vec::Vec::with_capacity(len);
        let vec_ptr = vec.spare_capacity_mut().as_mut_ptr();
        unsafe {
            #[allow(clippy::arithmetic_side_effects)]
            reader.read_exact(vec_ptr as *mut u8, len * size_of::<T>())?;
            // SAFETY: Caller ensures `T` is plain ol' data and can be initialized by raw byte reads.
            vec.set_len(len);
            // SAFETY: Caller ensures `ptr` is a valid ptr to a Vec<T>.
            ptr::write(dst, vec);
        }
        Ok(())
    }
}

impl<T, Len> SchemaWrite for BoxedSlice<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Src = Box<[T]>;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> Result<usize> {
        #[allow(clippy::arithmetic_side_effects)]
        Ok(Len::bytes_needed(src.len())? + size_of_val(&src[..]))
    }

    #[inline(always)]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        Len::encode_len(writer, src.len())?;
        // SAFETY: Caller ensures `T` is plain ol' data.
        unsafe { writer.write_exact(src[..].as_ptr() as *const u8, size_of_val(&src[..])) }
    }
}

impl<T, Len> SchemaRead for BoxedSlice<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Dst = Box<[T]>;

    #[inline(always)]
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        let mut vec = MaybeUninit::uninit();
        unsafe {
            // Leverage drop safety of the Vec impl.
            <Vec<Pod<T>, Len>>::read(reader, vec.as_mut_ptr())?;
            ptr::write(dst, vec.assume_init().into_boxed_slice());
        }
        Ok(())
    }
}

impl<T, Len> SchemaWrite for BoxedSlice<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = Box<[T::Src]>;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> Result<usize> {
        size_of_elem_iter::<T, Len>(src.iter())
    }

    #[inline(always)]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        write_elem_iter::<T, Len>(writer, src.iter())
    }
}

impl<T, Len> SchemaRead for BoxedSlice<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaRead,
{
    type Dst = Box<[T::Dst]>;

    #[inline(always)]
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        let mut v = MaybeUninit::uninit();
        unsafe {
            <Vec<Elem<T>, Len>>::read(reader, v.as_mut_ptr())?;
            ptr::write(dst, v.assume_init().into_boxed_slice());
        }
        Ok(())
    }
}

impl<T, Len> SchemaWrite for VecDeque<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Src = std::collections::VecDeque<T>;

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
            writer.write_exact(front.as_ptr() as *const u8, size_of_val(front))?;
            writer.write_exact(back.as_ptr() as *const u8, size_of_val(back))?;
        }
        Ok(())
    }
}

impl<T, Len> SchemaRead for VecDeque<Pod<T>, Len>
where
    Len: SeqLen,
{
    type Dst = std::collections::VecDeque<T>;

    #[inline(always)]
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        let mut vec = MaybeUninit::uninit();
        // Leverage the contiguous read optimization of `Vec`.
        <Vec<Pod<T>, Len>>::read(reader, vec.as_mut_ptr())?;
        unsafe {
            // From<Vec<T>> for VecDeque<T> is basically free.
            //
            // SAFETY:
            // - Caller ensures `dst` is a valid pointer to a VecDeque<T>.
            // - Caller ensures the given `SchemaRead` properly initializes the dst.
            ptr::write(dst, vec.assume_init().into());
        }

        Ok(())
    }
}

impl<T, Len> SchemaWrite for VecDeque<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = std::collections::VecDeque<T::Src>;

    #[inline(always)]
    fn size_of(value: &Self::Src) -> Result<usize> {
        size_of_elem_iter::<T, Len>(value.iter())
    }

    #[inline(always)]
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        write_elem_iter::<T, Len>(writer, src.iter())
    }
}

impl<T, Len> SchemaRead for VecDeque<Elem<T>, Len>
where
    Len: SeqLen,
    T: SchemaRead,
{
    type Dst = std::collections::VecDeque<T::Dst>;

    #[inline(always)]
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        let mut vec = MaybeUninit::uninit();
        // Leverage the contiguous read optimization of `Vec`.
        <Vec<Elem<T>, Len>>::read(reader, vec.as_mut_ptr())?;
        unsafe {
            // From<Vec<T>> for VecDeque<T> is basically free.
            // SAFETY:
            // - `dst` must be a valid pointer to a VecDeque<T>.
            // - The given `SchemaRead` must properly initialize the dst.
            ptr::write(dst, vec.assume_init().into());
        }

        Ok(())
    }
}
