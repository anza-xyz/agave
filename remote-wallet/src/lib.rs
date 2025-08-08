#![allow(clippy::arithmetic_side_effects)]
#![allow(dead_code)]

pub mod errors;
pub mod locator;
pub mod remote_keypair;
pub mod remote_wallet;
pub mod transport;
pub mod wallet;

pub trait Transport: Send {
    fn write(&self, data: &[u8]) -> Result<usize, String>;
    fn read(&self) -> Result<Vec<u8>, String>;
}
