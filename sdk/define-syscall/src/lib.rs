pub mod codes;
pub mod definitions;

#[cfg(target_feature = "static-syscalls")]
#[macro_export]
macro_rules! define_syscall {
    (fn $name:ident($($arg:ident: $typ:ty),*) -> $ret:ty, $code:ident) => {
        #[inline]
        pub unsafe fn $name($($arg: $typ),*) -> $ret {
            let syscall: extern "C" fn($($arg: $typ),*) -> $ret = core::mem::transmute($code as usize);
            syscall($($arg),*)
        }
    };
    (fn $name:ident($($arg:ident: $typ:ty),*), $code:ident) => {
        define_syscall!(fn $name($($arg: $typ),*) -> (), $code);
    }
}

#[cfg(not(target_feature = "static-syscalls"))]
#[macro_export]
macro_rules! define_syscall {
    (fn $name:ident($($arg:ident: $typ:ty),*) -> $ret:ty, $code:ident) => {
        extern "C" {
            pub fn $name($($arg: $typ),*) -> $ret;
        }
    };
    (fn $name:ident($($arg:ident: $typ:ty),*), $code:ident) => {
        define_syscall!(fn $name($($arg: $typ),*) -> (), $code);
    }
}
