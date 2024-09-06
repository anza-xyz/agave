// This code has been copied and modified from the generic-array crate.
use {
    crate::{Signature, SIGNATURE_BYTES},
    core::{
        fmt,
        mem::{forget, MaybeUninit},
        ptr, slice,
    },
    serde::{
        de::{Error, SeqAccess, Visitor},
        Deserialize, Deserializer,
    },
};

struct SignatureVisitor;

struct IntrusiveArrayBuilder<'a> {
    array: &'a mut [MaybeUninit<u8>; SIGNATURE_BYTES],
    position: usize,
}

impl<'a> IntrusiveArrayBuilder<'a> {
    /// Begin building an array
    #[inline(always)]
    fn new(array: &'a mut [MaybeUninit<u8>; SIGNATURE_BYTES]) -> IntrusiveArrayBuilder {
        IntrusiveArrayBuilder { array, position: 0 }
    }

    /// Returns true if the write position equals the array size
    #[inline(always)]
    fn is_full(&self) -> bool {
        self.position == SIGNATURE_BYTES
    }

    /// Creates a mutable iterator for writing to the array elements.
    ///
    /// You MUST increment the position value (given as a mutable reference) as you iterate
    /// to mark how many elements have been created.
    #[inline(always)]
    unsafe fn iter_position(&mut self) -> (slice::IterMut<MaybeUninit<u8>>, &mut usize) {
        (self.array.iter_mut(), &mut self.position)
    }

    /// When done writing (assuming all elements have been written to),
    /// get the inner array.
    #[inline(always)]
    unsafe fn finish(self) {
        debug_assert!(self.is_full());
        forget(self)
    }

    #[inline(always)]
    unsafe fn array_assume_init(
        array: [MaybeUninit<u8>; SIGNATURE_BYTES],
    ) -> [u8; SIGNATURE_BYTES] {
        ptr::read(&array as *const _ as *const MaybeUninit<[u8; SIGNATURE_BYTES]>).assume_init()
    }
}

impl<'a> Drop for IntrusiveArrayBuilder<'a> {
    fn drop(&mut self) {
        unsafe {
            ptr::drop_in_place(
                // Same cast as MaybeUninit::slice_assume_init_mut
                self.array.get_unchecked_mut(..self.position) as *mut [MaybeUninit<u8>]
                    as *mut [u8],
            );
        }
    }
}

// to avoid extra computation when testing for extra elements in the sequence
struct Dummy;
impl<'de> Deserialize<'de> for Dummy {
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Dummy)
    }
}

impl<'de> Visitor<'de> for SignatureVisitor {
    type Value = Signature;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("Signature")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        match seq.size_hint() {
            Some(n) if n != SIGNATURE_BYTES => {
                return Err(Error::invalid_length(n, &self));
            }
            _ => {}
        }

        unsafe {
            let mut dst: [MaybeUninit<u8>; SIGNATURE_BYTES] =
                [const { MaybeUninit::uninit() }; SIGNATURE_BYTES];
            let mut builder = IntrusiveArrayBuilder::new(&mut dst);

            let (build_iter, position) = builder.iter_position();

            for dst in build_iter {
                match seq.next_element()? {
                    Some(el) => {
                        dst.write(el);
                        *position += 1;
                    }
                    None => break,
                }
            }

            if *position == SIGNATURE_BYTES {
                if seq.size_hint() != Some(0) && seq.next_element::<Dummy>()?.is_some() {
                    return Err(Error::invalid_length(*position + 1, &self));
                }

                return Ok({
                    builder.finish();
                    Signature(IntrusiveArrayBuilder::array_assume_init(dst))
                });
            }

            Err(Error::invalid_length(*position, &self))
        }
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D>(deserializer: D) -> Result<Signature, D::Error>
    where
        D: Deserializer<'de>,
    {
        let visitor = SignatureVisitor;
        deserializer.deserialize_tuple(SIGNATURE_BYTES, visitor)
    }
}
