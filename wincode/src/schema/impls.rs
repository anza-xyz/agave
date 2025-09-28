use {
    crate::{
        error::{
            invalid_bool_encoding, invalid_tag_encoding, pointer_sized_decode_error, Error, Result,
        },
        io::{Reader, Writer},
        len::BincodeLen,
        schema::{
            containers::{self, Elem},
            size_of_elem_iter, write_elem_iter, SchemaRead, SchemaWrite,
        },
    },
    std::{collections::VecDeque, mem::MaybeUninit, ptr},
};

macro_rules! impl_int {
    ($type:ty) => {
        impl SchemaWrite for $type {
            type Src = $type;

            #[inline(always)]
            fn size_of(_src: &Self::Src) -> Result<usize> {
                Ok(size_of::<$type>())
            }

            #[inline(always)]
            fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
                // bincode defaults to little endian encoding.
                // noop on LE machines.
                unsafe {
                    // SAFETY: int is plain ol' data.
                    writer.write_t(&src.to_le_bytes())?;
                }
                Ok(())
            }
        }

        impl SchemaRead for $type {
            type Dst = $type;

            #[inline(always)]
            unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
                // SAFETY: Caller ensures `ptr` is a valid pointer to a u64.
                unsafe { reader.read_t(dst) }?;
                // bincode defaults to little endian encoding.
                #[cfg(target_endian = "big")]
                {
                    // SAFETY: `ptr` is initialized by `read_t`.
                    let val = unsafe { &mut *dst };
                    *val = val.swap_bytes();
                }

                Ok(())
            }
        }
    };

    ($type:ty as $cast:ty) => {
        impl SchemaWrite for $type {
            type Src = $type;

            #[inline(always)]
            fn size_of(_src: &Self::Src) -> Result<usize> {
                Ok(size_of::<$cast>())
            }

            #[inline(always)]
            fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
                let src = *src as $cast;
                // bincode defaults to little endian encoding.
                // noop on LE machines.
                unsafe {
                    // SAFETY: int is plain ol' data.
                    writer.write_t(&src.to_le_bytes())?;
                }
                Ok(())
            }
        }

        impl SchemaRead for $type {
            type Dst = $type;

            #[inline(always)]
            unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
                let casted = reader.get_t::<$cast>()?;
                let val = <$type>::from_le(
                    casted
                        .try_into()
                        .map_err(|_| pointer_sized_decode_error())?,
                );

                unsafe {
                    ptr::write(dst, val);
                }

                Ok(())
            }
        }
    };
}

impl_int!(u16);
impl_int!(i16);
impl_int!(u32);
impl_int!(i32);
impl_int!(u64);
impl_int!(i64);
impl_int!(u128);
impl_int!(i128);
impl_int!(usize as u64);
impl_int!(isize as i64);

macro_rules! impl_pod {
    ($type:ty) => {
        impl SchemaWrite for $type {
            type Src = $type;

            #[inline(always)]
            fn size_of(_src: &Self::Src) -> Result<usize> {
                Ok(size_of::<$type>())
            }

            #[inline(always)]
            fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
                // SAFETY: `$type` is plain ol' data.
                unsafe { writer.write_t(src) }
            }
        }

        impl SchemaRead for $type {
            type Dst = $type;

            #[inline(always)]
            unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
                // SAFETY: `$type` is plain ol' data.
                unsafe { reader.read_t(dst) }
            }
        }
    };
}

impl_pod!(u8);
impl_pod!(i8);

impl SchemaWrite for bool {
    type Src = bool;

    fn size_of(_src: &Self::Src) -> Result<usize> {
        Ok(size_of::<u8>())
    }

    fn write(writer: &mut Writer, src: &Self::Src) -> Result<()> {
        unsafe { writer.write_t(&(*src as u8)) }
    }
}

impl SchemaRead for bool {
    type Dst = bool;

    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        let byte = reader.get_t::<u8>()?;
        match byte {
            0 => unsafe { ptr::write(dst, false) },
            1 => unsafe { ptr::write(dst, true) },
            _ => return Err(invalid_bool_encoding(byte)),
        }
        Ok(())
    }
}

/// Blanket `Vec<T>` `SchemaWrite` implementation.
///
/// If the user wants to serialize a plain `Vec<T>`, we can assume it should
/// use the default bincode length encoding, as otherwise specifying a length
/// encoding would require a newtype struct with a custom #[serde(with = "x")],
/// which is obviously not the case here.
///
/// We also cannot make any assumptions about the "Plain Old Data" nature of `T`,
/// so we play it safe and use `Elem<T>` for the sequence mode.
impl<T> SchemaWrite for Vec<T>
where
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = Vec<T::Src>;

    #[inline(always)]
    fn size_of(value: &Self::Src) -> Result<usize> {
        <containers::Vec<Elem<T>, BincodeLen>>::size_of(value)
    }

    #[inline(always)]
    fn write(writer: &mut Writer, value: &Self::Src) -> Result<()> {
        <containers::Vec<Elem<T>, BincodeLen>>::write(writer, value)
    }
}

impl<T> SchemaRead for Vec<T>
where
    T: SchemaRead,
{
    type Dst = Vec<T::Dst>;

    #[inline(always)]
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        <containers::Vec<Elem<T>, BincodeLen>>::read(reader, dst)
    }
}

impl<T> SchemaWrite for VecDeque<T>
where
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = VecDeque<T::Src>;

    #[inline(always)]
    fn size_of(value: &Self::Src) -> Result<usize> {
        <containers::VecDeque<Elem<T>, BincodeLen>>::size_of(value)
    }

    #[inline(always)]
    fn write(writer: &mut Writer, value: &Self::Src) -> Result<()> {
        <containers::VecDeque<Elem<T>, BincodeLen>>::write(writer, value)
    }
}

impl<T> SchemaRead for VecDeque<T>
where
    T: SchemaRead,
{
    type Dst = VecDeque<T::Dst>;

    #[inline(always)]
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        <containers::VecDeque<Elem<T>, BincodeLen>>::read(reader, dst)
    }
}

/// Blanket `[T]` `SchemaWrite` implementation.
///
/// Similar to `Vec<T>`, we assume the default bincode length encoding and
/// can't make any assumptions about the "Plain Old Data" nature of `T`.
impl<T> SchemaWrite for [T]
where
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = [T::Src];

    #[inline(always)]
    fn size_of(value: &Self::Src) -> Result<usize> {
        size_of_elem_iter::<T, BincodeLen>(value.iter())
    }

    #[inline(always)]
    fn write(writer: &mut Writer, value: &Self::Src) -> Result<()> {
        write_elem_iter::<T, BincodeLen>(writer, value.iter())
    }
}

impl<T, const N: usize> SchemaRead for [T; N]
where
    T: SchemaRead,
{
    type Dst = [T::Dst; N];

    #[inline(always)]
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        let ptr = dst as *mut T::Dst;
        for i in 0..N {
            unsafe {
                // SAFETY:
                // - `i` is gated by `N`.
                // - `ptr` must be a valid pointer to a `[T::Dst; N]`.
                // - `T::read` must properly initialize the `T::Dst`.
                T::read(reader, ptr.add(i))?;
            }
        }
        Ok(())
    }
}

impl<T, const N: usize> SchemaWrite for [T; N]
where
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = [T::Src; N];

    #[inline(always)]
    fn size_of(value: &Self::Src) -> Result<usize> {
        #[allow(clippy::arithmetic_side_effects)]
        value
            .iter()
            .map(T::size_of)
            .try_fold(0, |acc, x| Ok::<_, Error>(acc + x?))
    }

    #[inline(always)]
    fn write(writer: &mut Writer, value: &Self::Src) -> Result<()> {
        for item in value {
            T::write(writer, item)?;
        }
        Ok(())
    }
}

impl<T> SchemaRead for Option<T>
where
    T: SchemaRead,
{
    type Dst = Option<T::Dst>;

    #[inline(always)]
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        let variant = u8::get(reader)?;
        match variant {
            0 => ptr::write(dst, Option::None),
            1 => {
                let mut value = MaybeUninit::uninit();
                unsafe {
                    // SAFETY:
                    // - `value` is a valid pointer to a `T::Dst`.
                    // - `T::read` must properly initialize the `T::Dst`.
                    T::read(reader, value.as_mut_ptr())?;
                    ptr::write(dst, Option::Some(value.assume_init()))
                }
            }
            _ => return Err(invalid_tag_encoding(variant as usize)),
        }

        Ok(())
    }
}

impl<T> SchemaWrite for Option<T>
where
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = Option<T::Src>;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> Result<usize> {
        #[allow(clippy::arithmetic_side_effects)]
        match src {
            Option::Some(value) => Ok(1 + T::size_of(value)?),
            Option::None => Ok(1),
        }
    }

    #[inline(always)]
    fn write(writer: &mut Writer, value: &Self::Src) -> Result<()> {
        match value {
            Option::Some(value) => {
                u8::write(writer, &1)?;
                T::write(writer, value)
            }
            Option::None => u8::write(writer, &0),
        }
    }
}

impl<'a, T> SchemaWrite for &'a T
where
    T: SchemaWrite,
{
    type Src = &'a T::Src;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> Result<usize> {
        T::size_of(src)
    }

    #[inline(always)]
    fn write(writer: &mut Writer, value: &Self::Src) -> Result<()> {
        T::write(writer, value)
    }
}

impl<T> SchemaWrite for Box<T>
where
    T: SchemaWrite,
{
    type Src = Box<T::Src>;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> Result<usize> {
        T::size_of(src)
    }

    #[inline(always)]
    fn write(writer: &mut Writer, value: &Self::Src) -> Result<()> {
        T::write(writer, value)
    }
}

impl<T> SchemaWrite for Box<[T]>
where
    T: SchemaWrite,
    T::Src: Sized,
{
    type Src = Box<[T::Src]>;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> Result<usize> {
        <containers::BoxedSlice<Elem<T>, BincodeLen>>::size_of(src)
    }

    #[inline(always)]
    fn write(writer: &mut Writer, value: &Self::Src) -> Result<()> {
        <containers::BoxedSlice<Elem<T>, BincodeLen>>::write(writer, value)
    }
}

impl<T> SchemaRead for Box<T>
where
    T: SchemaRead,
{
    type Dst = Box<T::Dst>;

    #[inline(always)]
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        let mut mem = Box::new_uninit();
        unsafe {
            // SAFETY:
            // - `mem` is a valid pointer to a `T::Dst`.
            // - `T::read` must properly initialize the `T::Dst`.
            T::read(reader, mem.as_mut_ptr())?;
            ptr::write(dst, mem.assume_init());
        }
        Ok(())
    }
}

impl<T> SchemaRead for Box<[T]>
where
    T: SchemaRead,
{
    type Dst = Box<[T::Dst]>;

    #[inline(always)]
    unsafe fn read(reader: &mut Reader, dst: *mut Self::Dst) -> Result<()> {
        <containers::BoxedSlice<Elem<T>, BincodeLen>>::read(reader, dst)
    }
}

/// Implement [`SchemaWrite`] and [`SchemaRead`] for a struct by specifying its constituent field schemas.
///
/// # Examples
///
/// ```
/// use wincode::compound;
///
/// struct Point {
///     x: u64,
///     y: u64,
/// }
///
/// compound! {
///     Point {
///         x: u64,
///         y: u64,
///     }
/// }
///
/// struct MyStruct {
///     point: Point,
/// }
///
/// compound! {
///     MyStruct {
///         point: Point,
///     }
/// }
/// ```
///
/// This macro also supports creating intermediate `#[repr(transparent)]` newtype structs that map to a target type.
/// This is useful when the target type is foreign (defined outside the crate).
///
/// ```
/// # use wincode::{containers::Pod, compound, Serialize, Deserialize};
/// // Imagine this is a struct defined outside our crate.
/// # #[derive(Debug, PartialEq, Eq)]
/// struct ForeignStruct {
///     pub field: [u8; 32],
/// }
///
/// compound! {
///     struct OurStruct => ForeignStruct {
///         field: Pod<[u8; 32]>,
///     }
/// }
///
/// let foreign_struct = ForeignStruct {
///     field: [0; 32],
/// };
/// let bytes = OurStruct::serialize(&foreign_struct).unwrap();
/// let deserialized = OurStruct::deserialize(&bytes).unwrap();
/// assert_eq!(foreign_struct, deserialized);
/// ```
///
/// Omitting the `struct` keyword will allow you to define the newtype yourself.
/// ```
///  # use wincode::{containers::Pod, compound, Serialize, Deserialize};
/// // Imagine this is a struct defined outside our crate.
/// # #[derive(Debug, PartialEq, Eq)]
/// struct ForeignStruct {
///     pub field: [u8; 32],
/// }
///
/// #[repr(transparent)]
/// struct OurStruct(ForeignStruct);
///
/// compound! {
///     OurStruct => ForeignStruct {
///         field: Pod<[u8; 32]>,
///     }
/// }
///
/// let foreign_struct = ForeignStruct {
///     field: [0; 32],
/// };
/// let bytes = OurStruct::serialize(&foreign_struct).unwrap();
/// let deserialized = OurStruct::deserialize(&bytes).unwrap();
/// assert_eq!(foreign_struct, deserialized);
/// ```
#[macro_export]
macro_rules! compound {
    ($vis:vis $src:ident { $($field:ident : $schema:ty),+ $(,)? }) => {
        $crate::compound! { $src => $src { $($field: $schema),+ } }
    };
    ($vis:vis struct $src:ident => $target:ty { $($field:ident : $schema:ty),+ $(,)? }) => {
        #[repr(transparent)]
        $vis struct $src($vis $target);

        $crate::compound! {
            $vis $src => $target {
                $($field: $schema),+
            }
        }
    };
    ($vis:vis $src:ident => $target:ty { $($field:ident : $schema:ty),+ $(,)? }) => {
        $crate::__private::paste! {
            #[allow(unused)]
            $vis trait [<$src Ext>] {
                $(
                    /// Read the field into the corresponding slot in `dst` from `reader`.
                    ///
                    /// # Safety
                    ///
                    /// - `dst` must be a valid pointer to a `Self`.
                    /// - Corresponding element schema must properly initialize the field.
                    unsafe fn [<read_ $field>](reader: &mut $crate::io::Reader, dst: *mut $target) -> $crate::error::Result<()>;
                )+
            }

            impl [<$src Ext>] for $src {
                #[inline(always)]
                $(unsafe fn [<read_ $field>](reader: &mut $crate::io::Reader, dst: *mut $target) -> $crate::error::Result<()> {
                    <$schema as $crate::SchemaRead>::read(reader, unsafe { &raw mut (*dst).$field })
                })+
            }
        }

        impl $crate::SchemaWrite for $src {
            type Src = $target;

            #[inline(always)]
            fn size_of(value: &Self::Src) -> $crate::error::Result<usize> {
                #[allow(clippy::arithmetic_side_effects)]
                Ok(0 $(+ <$schema as $crate::SchemaWrite>::size_of(&value.$field)?)+)
            }


            #[inline(always)]
            fn write(writer: &mut $crate::io::Writer, value: &Self::Src) -> $crate::error::Result<()> {
                $(<$schema as $crate::SchemaWrite>::write(writer, &value.$field)?;)+
                Ok(())
            }
        }

        impl $crate::SchemaRead for $src {
            type Dst = $target;

            /// Read the fields of `Self` from `reader` into `value`.
            ///
            /// # Safety
            ///
            /// - `value` must be a valid pointer to a `Self`.
            /// - Constituent element schemas must properly initialize elements.
            #[inline(always)]
            unsafe fn read(reader: &mut $crate::io::Reader, value: *mut Self::Dst) -> $crate::error::Result<()> {
                $(<$schema as $crate::SchemaRead>::read(reader, unsafe { &raw mut (*value).$field })?;)+
                Ok(())
            }
        }
    };
}

macro_rules! impl_tuple {
    ($($name:ident: $field:tt),+) => {
        impl<$($name),+> $crate::SchemaWrite for ($($name),+)
        where
            $($name: $crate::SchemaWrite),+,
            $($name::Src: Sized),+
        {
            type Src = ($($name::Src),+);

            #[inline(always)]
            fn size_of(value: &Self::Src) -> $crate::error::Result<usize> {
                #[allow(clippy::arithmetic_side_effects)]
                Ok(0 $(+ <$name as $crate::SchemaWrite>::size_of(&value.$field)?)+)
            }

            #[inline(always)]
            fn write(writer: &mut $crate::io::Writer, value: &Self::Src) -> $crate::error::Result<()> {
                $(<$name as $crate::SchemaWrite>::write(writer, &value.$field)?;)+
                Ok(())
            }
        }

        impl<$($name),+> $crate::SchemaRead for ($($name),+)
        where
            $($name: $crate::SchemaRead),+,
        {
            type Dst = ($($name::Dst),+);

            #[inline(always)]
            unsafe fn read(reader: &mut $crate::io::Reader, value: *mut Self::Dst) -> $crate::error::Result<()> {
                $(<$name as $crate::SchemaRead>::read(reader, unsafe { &raw mut (*value).$field })?;)+
                Ok(())
            }
        }
    };
}

impl_tuple! { A: 0, B: 1 }
impl_tuple! { A: 0, B: 1, C: 2 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4, F: 5 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8, J: 9 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8, J: 9, K: 10 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8, J: 9, K: 10, L: 11 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8, J: 9, K: 10, L: 11, M: 12 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8, J: 9, K: 10, L: 11, M: 12, N: 13 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8, J: 9, K: 10, L: 11, M: 12, N: 13, O: 14 }
impl_tuple! { A: 0, B: 1, C: 2, D: 3, E: 4, F: 5, G: 6, H: 7, I: 8, J: 9, K: 10, L: 11, M: 12, N: 13, O: 14, P: 15 }
