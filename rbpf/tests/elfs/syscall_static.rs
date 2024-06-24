mod syscalls;

#[no_mangle]
pub fn entrypoint() -> u64 {
    unsafe { syscalls::log(b"foo\n".as_ptr(), 4); }
    return 0;
}
