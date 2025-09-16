
pub mod fixture;
pub mod instr;
pub mod program_cache;
pub mod sysvar_cache;

use {
    fixture::proto::InstrContext as ProtoInstrContext,
    prost::Message,
    std::{env, ffi::c_int},
};

#[no_mangle]
pub unsafe extern "C" fn sol_compat_init(_log_level: i32) {
    env::set_var("SOLANA_RAYON_THREADS", "1");
    env::set_var("RAYON_NUM_THREADS", "1");
    if env::var("ENABLE_SOLANA_LOGGER").is_ok() {
        /* Pairs with RUST_LOG={trace,debug,info,etc} */
        solana_logger::setup();
    }
}

#[no_mangle]
pub unsafe extern "C" fn sol_compat_fini() {}

#[no_mangle]
pub unsafe extern "C" fn sol_compat_instr_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    let in_slice = std::slice::from_raw_parts(in_ptr, in_sz as usize);
    let Ok(instr_context) = ProtoInstrContext::decode(in_slice) else {
        return 0;
    };
    let Some(instr_effects) = instr::execute_instr_proto(instr_context) else {
        return 0;
    };
    let out_slice = std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize);
    let out_vec = instr_effects.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    *out_psz = out_vec.len() as u64;

    1
}
