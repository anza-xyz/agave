mod syscalls {
    extern "C" {
        pub fn log(str: *const u8, len: u64);
    }
}

#[no_mangle]
pub fn entrypoint() -> u64 {
    unsafe { syscalls::log(b"foo\n".as_ptr(), 4); }
    return 0;
}
