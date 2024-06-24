static mut VAL: u64 = 42;

#[no_mangle]
pub fn entrypoint() -> u64 {
    unsafe { core::ptr::write_volatile(&mut VAL, 0); }
    return 0;
}
