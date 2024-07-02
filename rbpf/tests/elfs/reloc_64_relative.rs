#[no_mangle]
pub fn entrypoint() -> u64 {
    return "entrypoint".as_ptr() as u64;
}
