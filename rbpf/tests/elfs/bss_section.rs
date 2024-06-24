static mut VAL: u64 = 0;

#[no_mangle]
pub fn entrypoint() -> u64 {
    unsafe { core::ptr::write_volatile(&mut VAL, 42); }
    return 0;
}
