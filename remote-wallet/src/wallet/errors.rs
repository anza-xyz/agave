use std::error::Error;

pub trait HardwareWalletError: Error {
    fn code(&self) -> u16;
    fn description(&self) -> String;
}
