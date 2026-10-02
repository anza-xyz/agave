use crate::{Error, Workload, check_output};

static DATA: [u8; 1000] = [8; 1000];
// Independent Keccak-f[1600] and Python hashlib.sha256 of bytes([8]) * 1000.
const KECCAK: [u8; 32] = [
    0x02, 0x77, 0x40, 0xbf, 0x2b, 0x7c, 0xd6, 0xc1, 0xda, 0xa7, 0x3d, 0x7f, 0x98, 0x55, 0xe9, 0x41,
    0xba, 0xd7, 0x78, 0x77, 0x1b, 0xd2, 0x2c, 0x26, 0x40, 0xbb, 0xce, 0x68, 0xe6, 0x84, 0x81, 0x2f,
];
const SHA256: [u8; 32] = [
    0x9b, 0x5e, 0x40, 0x7e, 0x7f, 0xf5, 0x11, 0xa7, 0x40, 0x11, 0x6c, 0x6d, 0x8e, 0x67, 0x8b, 0x03,
    0x49, 0xc3, 0x92, 0x59, 0xd2, 0x4d, 0x36, 0x9b, 0x12, 0x49, 0x1e, 0xd0, 0x6b, 0x1d, 0x4b, 0x87,
];

struct Hash {
    slices: Vec<&'static [u8]>,
    keccak: bool,
    output: [u8; 32],
}

pub fn keccak<const COUNT: usize, const SIZE: usize>() -> Box<dyn Workload> {
    Box::new(Hash {
        slices: vec![&DATA[..SIZE]; COUNT],
        keccak: true,
        output: [0; 32],
    })
}

pub fn sha256() -> Box<dyn Workload> {
    Box::new(Hash {
        slices: vec![&DATA],
        keccak: false,
        output: [0; 32],
    })
}

impl Workload for Hash {
    fn invoke_and_check(&mut self) -> Result<(), Error> {
        // Poison every output so a missing write cannot reuse the previous answer.
        self.output.fill(0xff);
        #[cfg(target_os = "solana")]
        let status = unsafe {
            // SAFETY: slice descriptors and their backing bytes remain live;
            // output has the syscall's required 32 writable bytes.
            if self.keccak {
                solana_define_syscall::definitions::sol_keccak256(
                    self.slices.as_ptr().cast(),
                    self.slices.len() as u64,
                    self.output.as_mut_ptr(),
                )
            } else {
                solana_define_syscall::definitions::sol_sha256(
                    self.slices.as_ptr().cast(),
                    self.slices.len() as u64,
                    self.output.as_mut_ptr(),
                )
            }
        };
        #[cfg(not(target_os = "solana"))]
        let status = {
            self.output = if self.keccak {
                solana_keccak_hasher::hashv(&self.slices).to_bytes()
            } else {
                solana_sha256_hasher::hashv(&self.slices).to_bytes()
            };
            0
        };
        check_output(
            status,
            &self.output,
            if self.keccak { &KECCAK } else { &SHA256 },
        )
    }
}
