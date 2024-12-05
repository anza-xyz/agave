//! Compile-time-safe division.

/// Convenience macro for doing integer division where the operation's safety
/// can be checked at compile-time.
///
/// Since `unchecked_div_by_const!()` is supposed to fail at compile-time, abuse
/// doctests to cover failure modes
///
/// # Examples
///
/// Literal denominator div-by-zero fails:
///
/// ```compile_fail
/// # use solana_unchecked_div_by_const::unchecked_div_by_const;
/// # fn main() {
/// let _ = unchecked_div_by_const!(10, 0);
/// # }
/// ```
///
/// Const denominator div-by-zero fails:
///
/// ```compile_fail
/// # use solana_unchecked_div_by_const::unchecked_div_by_const;
/// # fn main() {
/// const D: u64 = 0;
/// let _ = unchecked_div_by_const!(10, D);
/// # }
/// ```
///
/// Non-const denominator fails:
///
/// ```compile_fail
/// # use solana_unchecked_div_by_const::unchecked_div_by_const;
/// # fn main() {
/// let d = 0;
/// let _ = unchecked_div_by_const!(10, d);
/// # }
/// ```
///
/// Literal denominator div-by-zero fails:
///
/// ```compile_fail
/// # use solana_unchecked_div_by_const::unchecked_div_by_const;
/// # fn main() {
/// const N: u64 = 10;
/// let _ = unchecked_div_by_const!(N, 0);
/// # }
/// ```
///
/// Const denominator div-by-zero fails:
///
/// ```compile_fail
/// # use solana_unchecked_div_by_const::unchecked_div_by_const;
/// # fn main() {
/// const N: u64 = 10;
/// const D: u64 = 0;
/// let _ = unchecked_div_by_const!(N, D);
/// # }
/// ```
///
/// Non-const denominator fails:
///
/// ```compile_fail
/// # use solana_unchecked_div_by_const::unchecked_div_by_const;
/// # fn main() {
/// # const N: u64 = 10;
/// let d = 0;
/// let _ = unchecked_div_by_const!(N, d);
/// # }
/// ```
///
/// Literal denominator div-by-zero fails:
///
/// ```compile_fail
/// # use solana_unchecked_div_by_const::unchecked_div_by_const;
/// # fn main() {
/// let n = 10;
/// let _ = unchecked_div_by_const!(n, 0);
/// # }
/// ```
///
/// Const denominator div-by-zero fails:
///
/// ```compile_fail
/// # use solana_unchecked_div_by_const::unchecked_div_by_const;
/// # fn main() {
/// let n = 10;
/// const D: u64 = 0;
/// let _ = unchecked_div_by_const!(n, D);
/// # }
/// ```
///
/// Non-const denominator fails:
///
/// ```compile_fail
/// # use solana_unchecked_div_by_const::unchecked_div_by_const;
/// # fn main() {
/// let n = 10;
/// let d = 0;
/// let _ = unchecked_div_by_const!(n, d);
/// # }
/// ```
#[macro_export]
macro_rules! unchecked_div_by_const {
    ($num:expr, $den:expr) => {{
        // Ensure the denominator is compile-time constant
        let _ = [(); ($den - $den) as usize];
        // Compile-time constant integer div-by-zero passes for some reason
        // when invoked from a compilation unit other than that where this
        // macro is defined. Do an explicit zero-check for now. Sorry about the
        // ugly error messages!
        // https://users.rust-lang.org/t/unexpected-behavior-of-compile-time-integer-div-by-zero-check-in-declarative-macro/56718
        let _ = [(); ($den as usize) - 1];
        #[allow(clippy::arithmetic_side_effects)]
        let quotient = $num / $den;
        quotient
    }};
}

#[cfg(test)]
mod tests {
    use super::unchecked_div_by_const;

    #[test]
    fn test_unchecked_div_by_const() {
        const D: u64 = 2;
        const N: u64 = 10;
        let n = 10;
        assert_eq!(unchecked_div_by_const!(10, 2), 5);
        assert_eq!(unchecked_div_by_const!(N, 2), 5);
        assert_eq!(unchecked_div_by_const!(n, 2), 5);
        assert_eq!(unchecked_div_by_const!(10, D), 5);
        assert_eq!(unchecked_div_by_const!(N, D), 5);
        assert_eq!(unchecked_div_by_const!(n, D), 5);
    }
}
