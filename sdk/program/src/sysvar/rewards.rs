//! This sysvar is deprecated and unused.

use crate::sysvar::Sysvar;
pub use solana_reserved_account_keys::sysvar::rewards::{check_id, id, ID};

crate::impl_sysvar_id!(Rewards);

#[repr(C)]
#[derive(Serialize, Deserialize, Debug, Default, PartialEq)]
pub struct Rewards {
    pub validator_point_value: f64,
    pub unused: f64,
}
impl Rewards {
    pub fn new(validator_point_value: f64) -> Self {
        Self {
            validator_point_value,
            unused: 0.0,
        }
    }
}

impl Sysvar for Rewards {}
