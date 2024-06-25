#[cfg(test)]
#[macro_export]
macro_rules! assert_error {
    ($result:expr, $($error:expr),+) => {
        assert!(format!("{:?}", $result).contains(&format!($($error),+)));
    }
}
