use {
    core::ffi::c_void,
    dlopen2::symbor::{SymBorApi, Symbol},
    log::*,
    std::{
        env,
        os::raw::{c_int, c_uint},
        path::PathBuf,
    },
};

#[repr(C)]
pub struct Elems {
    pub elems: *const u8,
    pub num: u32,
}

#[derive(SymBorApi)]
pub struct Api<'a> {
    pub ed25519_init: Symbol<'a, unsafe extern "C" fn() -> bool>,
    pub ed25519_set_verbose: Symbol<'a, unsafe extern "C" fn(val: bool)>,

    #[allow(clippy::type_complexity)]
    pub ed25519_verify_many: Symbol<
        'a,
        unsafe extern "C" fn(
            vecs: *const Elems,
            num: u32,          //number of vecs
            message_size: u32, //size of each element inside the elems field of the vec
            total_packets: u32,
            total_signatures: u32,
            message_lens: *const u32,
            pubkey_offsets: *const u32,
            signature_offsets: *const u32,
            signed_message_offsets: *const u32,
            out: *mut u8, //combined length of all the items in vecs
            use_non_default_stream: u8,
        ) -> u32,
    >,

    #[allow(clippy::type_complexity)]
    pub ed25519_sign_many: Symbol<
        'a,
        unsafe extern "C" fn(
            vecs: *mut Elems,
            num: u32,          //number of vecs
            message_size: u32, //size of each element inside the elems field of the vec
            total_packets: u32,
            total_signatures: u32,
            message_lens: *const u32,
            pubkey_offsets: *const u32,
            privkey_offsets: *const u32,
            signed_message_offsets: *const u32,
            sgnatures_out: *mut u8, //combined length of all the items in vecs
            use_non_default_stream: u8,
        ) -> u32,
    >,

    pub poh_verify_many: Symbol<
        'a,
        unsafe extern "C" fn(
            hashes: *mut u8,
            num_hashes_arr: *const u64,
            num_elems: usize,
            use_non_default_stream: u8,
        ) -> c_int,
    >,

    pub cuda_host_register:
        Symbol<'a, unsafe extern "C" fn(ptr: *mut c_void, size: usize, flags: c_uint) -> c_int>,

    pub cuda_host_unregister: Symbol<'a, unsafe extern "C" fn(ptr: *mut c_void) -> c_int>,

    pub ed25519_get_checked_scalar:
        Symbol<'a, unsafe extern "C" fn(out_scalar: *mut u8, in_scalar: *const u8) -> c_int>,

    pub ed25519_check_packed_ge_small_order:
        Symbol<'a, unsafe extern "C" fn(packed_ge: *const u8) -> c_int>,
}

pub fn locate_perf_libs() -> Option<PathBuf> {
    let exe = env::current_exe().expect("Unable to get executable path");
    let perf_libs = exe.parent().unwrap().join("perf-libs");
    if perf_libs.is_dir() {
        info!("perf-libs found at {:?}", perf_libs);
        return Some(perf_libs);
    }
    warn!("{:?} does not exist", perf_libs);
    None
}

pub fn append_to_ld_library_path(mut ld_library_path: String) {
    if let Ok(env_value) = env::var("LD_LIBRARY_PATH") {
        ld_library_path.push(':');
        ld_library_path.push_str(&env_value);
    }
    info!("setting ld_library_path to: {:?}", ld_library_path);
    env::set_var("LD_LIBRARY_PATH", ld_library_path);
}
