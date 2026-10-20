//! Defines some wrapper types for manage stake

use {
    crate::fraction::Fraction,
    std::{fmt::Display, num::NonZero},
};

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
/// A wrapper to help distinguish u64 types from actual stake.
pub struct Stake(pub u64);

impl Stake {
    /// Increments self by rhs.
    pub fn increment(&mut self, rhs: Self) {
        self.0 = self.0.saturating_add(rhs.0);
    }

    /// Decrements self by rhs.
    pub fn decrement(&mut self, rhs: Self) {
        self.0 = self.0.saturating_sub(rhs.0);
    }

    #[must_use]
    #[allow(clippy::should_implement_trait)]
    /// Add rhs to self.
    pub fn add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }

    #[must_use]
    #[allow(clippy::should_implement_trait)]
    /// Subtracts rhs from self.
    pub fn sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl From<NonZeroStake> for Stake {
    fn from(value: NonZeroStake) -> Self {
        Self(value.0.get())
    }
}

impl Display for Stake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
/// A wrapper to help distinguish non zero u64 types from actual stake.
pub struct NonZeroStake(NonZero<u64>);

impl NonZeroStake {
    /// Constructs a NonZeroStake from a u64.
    pub const fn new(val: u64) -> Option<Self> {
        match NonZero::new(val) {
            None => None,
            Some(val) => Some(Self(val)),
        }
    }

    /// Returns the internal NonZero.
    pub fn get(self) -> NonZero<u64> {
        self.0
    }

    /// Returns a `Fraction` with self as denominator.
    pub fn fraction(self, stake: Stake) -> Fraction {
        Fraction::new(stake.0, self.0)
    }

    /// Increments self by rhs.
    pub fn increment(&mut self, rhs: Self) {
        self.0 = self.0.saturating_add(rhs.0.get());
    }

    #[must_use]
    #[allow(clippy::should_implement_trait)]
    /// Add rhs to self.
    pub fn add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0.get()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stake_addition() {
        for (lhs, rhs, expected) in [
            (0, 0, 0),
            (10, 0, 10),
            (0, 10, 10),
            (10, 20, 30),
            (u64::MAX - 1, 1, u64::MAX),
            (u64::MAX - 1, 2, u64::MAX),
            (u64::MAX, u64::MAX, u64::MAX),
        ] {
            let stake = Stake(lhs);
            assert_eq!(stake.add(Stake(rhs)), Stake(expected));
            assert_eq!(stake, Stake(lhs));

            let mut stake = stake;
            stake.increment(Stake(rhs));
            assert_eq!(stake, Stake(expected));
        }
    }

    #[test]
    fn test_stake_subtraction() {
        for (lhs, rhs, expected) in [
            (0, 0, 0),
            (10, 0, 10),
            (30, 20, 10),
            (20, 20, 0),
            (10, 20, 0),
            (0, u64::MAX, 0),
            (u64::MAX, 1, u64::MAX - 1),
            (u64::MAX, u64::MAX, 0),
        ] {
            let stake = Stake(lhs);
            assert_eq!(stake.sub(Stake(rhs)), Stake(expected));
            assert_eq!(stake, Stake(lhs));

            let mut stake = stake;
            stake.decrement(Stake(rhs));
            assert_eq!(stake, Stake(expected));
        }
    }

    #[test]
    fn test_stake_default() {
        assert_eq!(Stake::default(), Stake(0));
    }

    #[test]
    fn test_nonzero_stake_new() {
        assert_eq!(NonZeroStake::new(0), None);
        for value in [1, 123, u64::MAX] {
            assert_eq!(NonZeroStake::new(value).unwrap().0.get(), value);
        }
    }

    #[test]
    fn test_stake_from_nonzero_stake() {
        for value in [1, 123, u64::MAX] {
            assert_eq!(Stake::from(NonZeroStake::new(value).unwrap()), Stake(value));
        }
    }

    #[test]
    fn test_nonzero_stake_addition() {
        for (lhs, rhs, expected) in [
            (1, 1, 2),
            (10, 20, 30),
            (u64::MAX - 1, 1, u64::MAX),
            (u64::MAX - 1, 2, u64::MAX),
            (u64::MAX, u64::MAX, u64::MAX),
        ] {
            let stake = NonZeroStake::new(lhs).unwrap();
            let rhs = NonZeroStake::new(rhs).unwrap();
            let expected = NonZeroStake::new(expected).unwrap();
            assert_eq!(stake.add(rhs), expected);
            assert_eq!(stake.0.get(), lhs);

            let mut stake = stake;
            stake.increment(rhs);
            assert_eq!(stake, expected);
        }
    }

    #[test]
    fn test_nonzero_stake_fraction() {
        let total = NonZeroStake::new(100).unwrap();
        assert_eq!(total.fraction(Stake(0)), Fraction::from_percentage(0));
        assert_eq!(total.fraction(Stake(40)), Fraction::from_percentage(40));
        assert_eq!(total.fraction(Stake(100)), Fraction::from_percentage(100));
        assert_eq!(total.fraction(Stake(150)), Fraction::from_percentage(150));

        let total = NonZeroStake::new(u64::MAX).unwrap();
        assert_eq!(
            total.fraction(Stake(u64::MAX - 1)),
            Fraction::new(u64::MAX - 1, NonZero::new(u64::MAX).unwrap())
        );
    }
}
