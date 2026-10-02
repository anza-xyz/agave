use crate::{Error, by_id, by_name};

pub const SCHEMA_VERSION: u32 = 1;
pub const INSTRUCTION_LEN: usize = 24;
pub const MAX_ITERATIONS: u32 = 1024;
const MAGIC: &[u8; 4] = b"SCAS";

/// Encoding: magic, schema version, case ID, iterations, nonce, all integers LE.
/// Bounded fixed-size decoding never allocates or silently clamps a parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub case_id: u32,
    pub iterations: u32,
    pub nonce: u64,
}

impl Request {
    pub fn new(name: &str, iterations: u32, nonce: u64) -> Result<Self, Error> {
        let request = Self {
            case_id: by_name(name).ok_or(Error::UnknownCase)?.id,
            iterations,
            nonce,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), Error> {
        if !(1..=MAX_ITERATIONS).contains(&self.iterations) {
            return Err(Error::InvalidIterations);
        }
        if by_id(self.case_id).is_none() {
            return Err(Error::UnknownCase);
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<[u8; INSTRUCTION_LEN], Error> {
        self.validate()?;
        let mut data = [0; INSTRUCTION_LEN];
        data[..4].copy_from_slice(MAGIC);
        data[4..8].copy_from_slice(&SCHEMA_VERSION.to_le_bytes());
        data[8..12].copy_from_slice(&self.case_id.to_le_bytes());
        data[12..16].copy_from_slice(&self.iterations.to_le_bytes());
        data[16..24].copy_from_slice(&self.nonce.to_le_bytes());
        Ok(data)
    }

    pub fn decode(data: &[u8]) -> Result<Self, Error> {
        if data.len() != INSTRUCTION_LEN || &data[..4] != MAGIC {
            return Err(Error::InvalidInstruction);
        }
        let read_u32 = |range: std::ops::Range<usize>| {
            u32::from_le_bytes(data[range].try_into().expect("checked instruction length"))
        };
        if read_u32(4..8) != SCHEMA_VERSION {
            return Err(Error::InvalidInstruction);
        }
        let request = Self {
            case_id: read_u32(8..12),
            iterations: read_u32(12..16),
            nonce: u64::from_le_bytes(data[16..24].try_into().expect("checked instruction length")),
        };
        request.validate()?;
        Ok(request)
    }
}
