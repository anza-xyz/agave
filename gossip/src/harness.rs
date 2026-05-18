use {crate::protocol::gossip_decode_to_effects, prost::Message, std::ffi::c_int};

/// Conformance harness entry point for gossip message decoding.
///
/// Deserializes a binary-encoded gossip message from the input buffer,
/// decodes it into effects, and writes the Protobuf-encoded result into
/// the output buffer.
///
/// # Parameters
/// - `out_ptr`: Pointer to the output buffer where the Protobuf-encoded
///   result will be written.
/// - `out_psz`: Pointer to a `u64` that on entry holds the capacity of the
///   output buffer and on successful return is updated to the number of
///   bytes actually written.
/// - `in_ptr`: Pointer to the input buffer containing the binary-encoded
///   gossip message. May be null when `in_sz` is 0.
/// - `in_sz`: Length of the input buffer in bytes.
///
/// # Returns
/// `1` on success, `0` on failure (null pointers, zero-length input, or
/// output buffer too small).
///
/// # Safety
/// - `out_ptr` must point to a writable buffer whose capacity is stored at
///   `*out_psz`.
/// - `out_psz` must be a valid, aligned pointer to a `u64`.
/// - `in_ptr` must point to a readable buffer of at least `in_sz` bytes
///   (or be null/ignored when `in_sz` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_gossip_decode_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *const u8,
    in_sz: u64,
) -> c_int {
    if out_ptr.is_null() || out_psz.is_null() {
        return 0;
    }

    let in_slice = if in_sz == 0 {
        &[]
    } else if in_ptr.is_null() {
        return 0;
    } else {
        unsafe { std::slice::from_raw_parts(in_ptr, in_sz as usize) }
    };

    let effects = gossip_decode_to_effects(in_slice);
    let out_vec = effects.encode_to_vec();

    let out_cap = unsafe { *out_psz } as usize;
    if out_vec.len() > out_cap {
        return 0;
    }
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, out_cap) };
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    unsafe { *out_psz = out_vec.len() as u64 };
    1
}
