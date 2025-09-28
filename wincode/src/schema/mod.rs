//! Schema definitions that make it simple to implement performant serialization
//! and deserialization of types, with bincode and [`solana_short_vec`] support.
//!
//! # Example
//!
//! ```
//! # use rand::prelude::*;
//! # use wincode::{compound, Serialize, Deserialize, len::{BincodeLen, ShortU16Len}, containers::{self, Pod}};
//! # use std::array;
//!
//! # #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
//! #[repr(transparent)]
//! struct Signature([u8; 32]);
//! # #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
//! #[repr(transparent)]
//! struct Address([u8; 32]);
//!
//! # #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
//! struct MyStruct {
//!     signature: Vec<Signature>,
//!     #[serde(with = "solana_short_vec")]
//!     address: Vec<Address>,
//! }
//!
//! compound! {
//!     MyStruct {
//!         signature: containers::Vec<Pod<Signature>, BincodeLen>,
//!         address: containers::Vec<Pod<Address>, ShortU16Len>,
//!     }
//! }
//!
//! let my_struct = MyStruct {
//!     signature: (0..10).map(|_| Signature(array::from_fn(|_| random()))).collect(),
//!     address: (0..10).map(|_| Address(array::from_fn(|_| random()))).collect(),
//! };
//! let bincode_serialized = bincode::serialize(&my_struct).unwrap();
//! let wincode_serialized = wincode::serialize(&my_struct).unwrap();
//! assert_eq!(bincode_serialized, wincode_serialized);
//!
//! let bincode_deserialized: MyStruct = bincode::deserialize(&bincode_serialized).unwrap();
//! let wincode_deserialized: MyStruct = wincode::deserialize(&wincode_serialized).unwrap();
//! assert_eq!(bincode_deserialized, wincode_deserialized);
//! ```
use {
    crate::{
        error::{Error, Result},
        io::*,
        len::SeqLen,
    },
    std::mem::MaybeUninit,
};

pub mod containers;
mod impls;

/// Types that can be written (serialized) to a byte buffer.
pub trait SchemaWrite {
    type Src: ?Sized;
    /// Get the serialized size of `Self::Src`.
    fn size_of(src: &Self::Src) -> Result<usize>;
    /// Write `Self::Src` to `writer`.
    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()>;
}

/// Types that can be read (deserialized) from a byte buffer.
pub trait SchemaRead {
    type Dst;
    /// Read into `dst` from `reader`.
    ///
    /// # Safety
    ///
    /// - `dst` must be a valid pointer to a `Self::Dst`.
    /// - `Self::Dst` must be plain ol' data.
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()>;
    /// Read `Self::Dst` from `reader` into a new `Self::Dst`.
    #[inline(always)]
    fn get(reader: &mut Reader) -> Result<Self::Dst> {
        let mut value = MaybeUninit::uninit();
        // SAFETY: `value` is a valid pointer to a `Self::Dst`.
        unsafe {
            Self::read(reader, value.as_mut_ptr())?;
            Ok(value.assume_init())
        }
    }
}

#[inline(always)]
fn size_of_elem_iter<'a, T, Len>(value: impl ExactSizeIterator<Item = &'a T::Src>) -> Result<usize>
where
    Len: SeqLen,
    T: SchemaWrite + 'a,
{
    #[allow(clippy::arithmetic_side_effects)]
    Ok(Len::bytes_needed(value.len())?
        + value
            .map(T::size_of)
            .try_fold(0, |acc, x| Ok::<_, Error>(acc + x?))?)
}

#[inline(always)]
fn write_elem_iter<'a, T, Len>(
    writer: &mut Writer,
    src: impl ExactSizeIterator<Item = &'a T::Src>,
) -> Result<()>
where
    Len: SeqLen,
    T: SchemaWrite + 'a,
{
    Len::encode_len(writer, src.len())?;
    for item in src {
        T::write(writer, item)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            compound,
            containers::{self, Elem, Pod},
            deserialize,
            len::BincodeLen,
            serialize, Deserialize, Serialize,
        },
        proptest::prelude::*,
        std::{collections::VecDeque, result::Result},
    };

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
    struct SomeStruct {
        a: u64,
        b: u64,
    }

    compound! {
        SomeStruct {
            a: u64,
            b: u64,
        }
    }

    fn strat_some_struct() -> impl Strategy<Value = SomeStruct> {
        (0..=u64::MAX, 0..=u64::MAX).prop_map(|(a, b)| SomeStruct { a, b })
    }

    fn strat_byte_array() -> impl Strategy<Value = [u8; 32]> {
        any::<[u8; 32]>()
    }

    fn strat_byte_vec() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(any::<u8>(), 0..=100)
    }

    proptest! {
        #[test]
        fn test_vec_elem(vec in proptest::collection::vec(strat_some_struct(), 0..=100)) {
            let bincode_serialized = bincode::serialize(&vec).unwrap();
            type Target = containers::Vec<Elem<SomeStruct>, BincodeLen>;
            let schema_serialized = Target::serialize(&vec).unwrap();

            prop_assert_eq!(&bincode_serialized, &schema_serialized);

            let bincode_deserialized: Vec<SomeStruct> = bincode::deserialize(&bincode_serialized).unwrap();
            let schema_deserialized = Target::deserialize(&schema_serialized).unwrap();
            prop_assert_eq!(&vec, &bincode_deserialized);
            prop_assert_eq!(vec, schema_deserialized);
        }

        #[test]
        fn test_vec_pod(vec in proptest::collection::vec(strat_byte_array(), 0..=100)) {
            let bincode_serialized = bincode::serialize(&vec).unwrap();
            type Target = containers::Vec<Pod<[u8; 32]>, BincodeLen>;
            let schema_serialized = Target::serialize(&vec).unwrap();

            prop_assert_eq!(&bincode_serialized, &schema_serialized);

            let bincode_deserialized: Vec<[u8; 32]> = bincode::deserialize(&bincode_serialized).unwrap();
            let schema_deserialized = Target::deserialize(&schema_serialized).unwrap();
            prop_assert_eq!(&vec, &bincode_deserialized);
            prop_assert_eq!(vec, schema_deserialized);
        }

        #[test]
        fn test_vec_deque_elem(vec in proptest::collection::vec_deque(strat_some_struct(), 0..=100)) {
            let bincode_serialized = bincode::serialize(&vec).unwrap();
            type Target = containers::VecDeque<Elem<SomeStruct>, BincodeLen>;
            let schema_serialized = Target::serialize(&vec).unwrap();

            prop_assert_eq!(&bincode_serialized, &schema_serialized);

            let bincode_deserialized: VecDeque<SomeStruct> = bincode::deserialize(&bincode_serialized).unwrap();
            let schema_deserialized = Target::deserialize(&schema_serialized).unwrap();
            prop_assert_eq!(&vec, &bincode_deserialized);
            prop_assert_eq!(vec, schema_deserialized);
        }

        #[test]
        fn test_array(array in strat_byte_array()) {
            let bincode_serialized = bincode::serialize(&array).unwrap();
            type Target = [u8; 32];
            let schema_serialized = Target::serialize(&array).unwrap();
            prop_assert_eq!(&bincode_serialized, &schema_serialized);
            let bincode_deserialized: [u8; 32] = bincode::deserialize(&bincode_serialized).unwrap();
            let schema_deserialized: [u8; 32] = deserialize(&schema_serialized).unwrap();
            prop_assert_eq!(&array, &bincode_deserialized);
            prop_assert_eq!(array, schema_deserialized);
        }

        #[test]
        fn test_option(option in proptest::option::of(strat_some_struct())) {
            let bincode_serialized = bincode::serialize(&option).unwrap();
            let schema_serialized = serialize(&option).unwrap();

            prop_assert_eq!(&bincode_serialized, &schema_serialized);
            let bincode_deserialized: Option<SomeStruct> = bincode::deserialize(&bincode_serialized).unwrap();
            let schema_deserialized: Option<SomeStruct> = deserialize(&schema_serialized).unwrap();
            prop_assert_eq!(&option, &bincode_deserialized);
            prop_assert_eq!(&option, &schema_deserialized);
        }

        #[test]
        fn test_option_container(option in proptest::option::of(strat_byte_array())) {
            let bincode_serialized = bincode::serialize(&option).unwrap();
            type Target = Option<Pod<[u8; 32]>>;
            let schema_serialized = Target::serialize(&option).unwrap();
            prop_assert_eq!(&bincode_serialized, &schema_serialized);
            let bincode_deserialized: Option<[u8; 32]> = bincode::deserialize(&bincode_serialized).unwrap();
            let schema_deserialized: Option<[u8; 32]> = Target::deserialize(&schema_serialized).unwrap();
            prop_assert_eq!(&option, &bincode_deserialized);
            prop_assert_eq!(&option, &schema_deserialized);
        }

        #[test]
        fn test_bool(val in any::<bool>()) {
            let bincode_serialized = bincode::serialize(&val).unwrap();
            let schema_serialized = serialize(&val).unwrap();
            prop_assert_eq!(&bincode_serialized, &schema_serialized);
            let bincode_deserialized: bool = bincode::deserialize(&bincode_serialized).unwrap();
            let schema_deserialized: bool = deserialize(&schema_serialized).unwrap();
            prop_assert_eq!(val, bincode_deserialized);
            prop_assert_eq!(val, schema_deserialized);
        }

        #[test]
        fn test_bool_invalid_bit_pattern(val in 2u8..=255) {
            let bincode_deserialized: Result<bool,_> = bincode::deserialize(&[val]);
            let schema_deserialized: Result<bool,_> = deserialize(&[val]);
            prop_assert!(bincode_deserialized.is_err());
            prop_assert!(schema_deserialized.is_err());
        }

        #[test]
        fn test_box(ar in strat_byte_array(), s in strat_some_struct()) {
            let data = (Box::new(ar), Box::new(s));
            let bincode_serialized = bincode::serialize(&data).unwrap();
            type Target = (Box<[u8; 32]>, Box<SomeStruct>);
            type SchemaPodTarget = (Box<Pod<[u8; 32]>>, Box<SomeStruct>);
            let schema_serialized = Target::serialize(&data).unwrap();
            let schema_pod_serialized = SchemaPodTarget::serialize(&data).unwrap();
            prop_assert_eq!(&bincode_serialized, &schema_serialized);
            prop_assert_eq!(&bincode_serialized, &schema_pod_serialized);
            let bincode_deserialized: (Box<[u8; 32]>, Box<SomeStruct>) = bincode::deserialize(&bincode_serialized).unwrap();
            let schema_deserialized: (Box<[u8; 32]>, Box<SomeStruct>) = Target::deserialize(&schema_serialized).unwrap();
            let schema_pod_deserialized: (Box<[u8; 32]>, Box<SomeStruct>) = SchemaPodTarget::deserialize(&schema_pod_serialized).unwrap();
            prop_assert_eq!(&data, &bincode_deserialized);
            prop_assert_eq!(&data, &schema_deserialized);
            prop_assert_eq!(&data, &schema_pod_deserialized);
        }

        #[test]
        fn test_boxed_slice(vec in strat_byte_vec()) {
            let data = vec.into_boxed_slice();
            let bincode_serialized = bincode::serialize(&data).unwrap();
            type Target = Box<[u8]>;
            type TargetPod = containers::BoxedSlice<Pod<u8>>;
            let schema_serialized = Target::serialize(&data).unwrap();
            let schema_pod_serialized = TargetPod::serialize(&data).unwrap();
            prop_assert_eq!(&bincode_serialized, &schema_serialized);
            prop_assert_eq!(&bincode_serialized, &schema_pod_serialized);
            let bincode_deserialized: Box<[u8]> = bincode::deserialize(&bincode_serialized).unwrap();
            let schema_deserialized: Box<[u8]> = Target::deserialize(&schema_serialized).unwrap();
            let schema_pod_deserialized: Box<[u8]> = TargetPod::deserialize(&schema_pod_serialized).unwrap();
            prop_assert_eq!(&data, &bincode_deserialized);
            prop_assert_eq!(&data, &schema_deserialized);
            prop_assert_eq!(&data, &schema_pod_deserialized);
        }

        #[test]
        fn test_integers(
            val in (
                any::<u8>(),
                any::<i8>(),
                any::<u16>(),
                any::<i16>(),
                any::<u32>(),
                any::<i32>(),
                any::<usize>(),
                any::<isize>(),
                any::<u64>(),
                any::<i64>(),
                any::<u128>(),
                any::<i128>()
            )
        ) {
            type Target = (u8, i8, u16, i16, u32, i32, usize, isize, u64, i64, u128, i128);
            let bincode_serialized = bincode::serialize(&val).unwrap();
            let schema_serialized = serialize(&val).unwrap();
            prop_assert_eq!(&bincode_serialized, &schema_serialized);
            let bincode_deserialized: Target = bincode::deserialize(&bincode_serialized).unwrap();
            let schema_deserialized: Target = deserialize(&schema_serialized).unwrap();
            prop_assert_eq!(val, bincode_deserialized);
            prop_assert_eq!(val, schema_deserialized);
        }

        #[test]
        fn test_tuple(
            tuple in (
                strat_some_struct(),
                strat_byte_array(),
                proptest::option::of(strat_some_struct()),
                proptest::collection::vec(strat_some_struct(), 0..=100),
                proptest::collection::vec_deque(strat_byte_array(), 0..=100)
            )
        ) {
            let bincode_serialized = bincode::serialize(&tuple).unwrap();
            type BincodeTarget = (SomeStruct, [u8; 32], Option<SomeStruct>, Vec<SomeStruct>, VecDeque<[u8; 32]>);
            type Target = (SomeStruct, Pod<[u8; 32]>, Option<SomeStruct>, Vec<SomeStruct>, containers::VecDeque<Pod<[u8; 32]>, BincodeLen>);
            let schema_serialized = Target::serialize(&tuple).unwrap();

            prop_assert_eq!(&bincode_serialized, &schema_serialized);
            let bincode_deserialized: BincodeTarget = bincode::deserialize(&bincode_serialized).unwrap();
            let schema_deserialized = Target::deserialize(&schema_serialized).unwrap();
            prop_assert_eq!(&tuple, &bincode_deserialized);
            prop_assert_eq!(&tuple, &schema_deserialized);

        }
    }
}
