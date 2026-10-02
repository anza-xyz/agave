//! Shared fixtures and request encoding for syscall stress tests.
//!
//! Add a workload adapter and a registry entry here; consumers select a case by
//! name and use [`Request::encode`] / [`execute`]. IDs and input definitions are
//! stable: allocate a new ID when a case changes. Consumers must build the same
//! revision and optional features as the deployed program.
//!
//! SBF execution calls actual syscalls and verifies their status and output.
//! Native execution uses SDK reference implementations for correctness checks;
//! it is NOT a native-handler benchmark. Transaction execution also includes
//! preparation, dispatch and output checks. These cases do not assign CU prices
//! or claim worst-case coverage. Simulate transactions before measuring load,
//! check success and consumed CU, and leave room for program overhead.

#![cfg(feature = "agave-unstable-api")]

mod hash;
#[cfg(feature = "modexp")]
mod modexp;
#[cfg(feature = "pairing")]
mod pairing;
mod request;

pub use request::{INSTRUCTION_LEN, MAX_ITERATIONS, Request, SCHEMA_VERSION};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidInstruction,
    UnknownCase,
    InvalidIterations,
    SyscallFailed(u64),
    OutputMismatch,
}

/// One prepared invocation, including its expected output. Implementations must
/// reuse their SBF allocations so repetitions do not exhaust the bump allocator.
pub trait Workload {
    fn invoke_and_check(&mut self) -> Result<(), Error>;
}

pub struct Case {
    pub id: u32,
    pub name: &'static str,
    pub syscall: &'static str,
    /// On-chain feature-account addresses, for the sender's preflight checks.
    /// Declaring a feature here does not activate it.
    pub required_features: &'static [&'static str],
    pub dimensions: &'static [(&'static str, u64)],
    pub prepare: fn() -> Box<dyn Workload>,
}

struct Noop;
impl Workload for Noop {
    fn invoke_and_check(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// This is a starting catalogue, not an exhaustive syscall suite.
pub static CASES: &[Case] = &[
    Case {
        id: 0,
        name: "noop",
        syscall: "",
        required_features: &[],
        dimensions: &[],
        prepare: || Box::new(Noop),
    },
    Case {
        id: 1,
        name: "keccak-1000-1",
        syscall: "sol_keccak256",
        required_features: &[],
        dimensions: &[("bytes", 1000), ("slices", 1)],
        prepare: hash::keccak::<1, 1000>,
    },
    Case {
        id: 2,
        name: "keccak-1000-100",
        syscall: "sol_keccak256",
        required_features: &[],
        dimensions: &[("bytes", 1000), ("slices", 100)],
        prepare: hash::keccak::<100, 10>,
    },
    Case {
        id: 3,
        name: "keccak-1000-1000",
        syscall: "sol_keccak256",
        required_features: &[],
        dimensions: &[("bytes", 1000), ("slices", 1000)],
        prepare: hash::keccak::<1000, 1>,
    },
    Case {
        id: 4,
        name: "sha256-1000",
        syscall: "sol_sha256",
        required_features: &[],
        dimensions: &[("bytes", 1000), ("slices", 1)],
        prepare: hash::sha256,
    },
    #[cfg(feature = "pairing")]
    Case {
        id: 100,
        name: "bn254-pairing-2",
        syscall: "sol_alt_bn128_group_op",
        required_features: &["A16q37opZdQMCbe5qJ6xpBB9usykfv8jZaMkxvZQi4GJ"],
        dimensions: &[("pairs", 2), ("bytes", 384)],
        prepare: pairing::prepare::<1>,
    },
    #[cfg(feature = "pairing")]
    Case {
        id: 101,
        name: "bn254-pairing-4",
        syscall: "sol_alt_bn128_group_op",
        required_features: &["A16q37opZdQMCbe5qJ6xpBB9usykfv8jZaMkxvZQi4GJ"],
        dimensions: &[("pairs", 4), ("bytes", 768)],
        prepare: pairing::prepare::<2>,
    },
    #[cfg(feature = "modexp")]
    Case {
        id: 200,
        name: "modexp-128-e65537",
        syscall: "sol_big_mod_exp",
        required_features: &["expH2ppKPW2ANEdEmAjfhSEcnBQJfmoX4FjuNpe9ttg"],
        dimensions: &[
            ("base_bytes", 128),
            ("exponent_bytes", 3),
            ("modulus_bytes", 128),
        ],
        prepare: modexp::prepare,
    },
];

pub fn by_name(name: &str) -> Option<&'static Case> {
    CASES.iter().find(|case| case.name == name)
}

pub fn by_id(id: u32) -> Option<&'static Case> {
    CASES.iter().find(|case| case.id == id)
}

/// Execute a versioned request; nonce affects transaction identity only.
/// No syscall failure, including budget exhaustion, is accepted as useful work.
pub fn execute(data: &[u8]) -> Result<(), Error> {
    let request = Request::decode(data)?;
    let case = by_id(request.case_id).ok_or(Error::UnknownCase)?;
    let mut workload = (case.prepare)();
    for _ in 0..request.iterations {
        workload.invoke_and_check()?;
    }
    Ok(())
}

fn check_output(status: u64, actual: &[u8], expected: &[u8]) -> Result<(), Error> {
    if status != 0 {
        Err(Error::SyscallFailed(status))
    } else if actual != expected {
        Err(Error::OutputMismatch)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonzero_status_and_wrong_output_are_never_success() {
        assert_eq!(check_output(7, &[1], &[1]), Err(Error::SyscallFailed(7)));
        assert_eq!(check_output(0, &[0], &[1]), Err(Error::OutputMismatch));
    }
}
