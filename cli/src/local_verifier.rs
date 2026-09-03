use solana_sbpf::ebpf;
use solana_sbpf::program::{FunctionRegistry, SBPFVersion};
use solana_sbpf::verifier::{Verifier, VerifierError};

// Verifier to run locally when someone invokes `solana deploy` and the related commands
#[derive(Debug)]
pub struct LocalVerifier {}

impl Verifier for LocalVerifier {
    /// Check if an SBPFv3 program contain any problematic instruction
    fn verify<T: Copy + PartialEq>(
        prog: &[u8],
        sbpf_version: SBPFVersion,
        syscall_registry: &FunctionRegistry<T>,
    ) -> Result<(), VerifierError> {
        if sbpf_version < SBPFVersion::V3 {
            // Nothing to check
            return Ok(());
        }

        let mut insn_ptr: usize = 0;
        while (insn_ptr + 1) * ebpf::INSN_SIZE <= prog.len() {
            let insn = ebpf::get_insn(prog, insn_ptr);

            match insn.opc {
                ebpf::LD_DW_IMM => {
                    insn_ptr += 1;
                }

                ebpf::CALL_IMM
                if insn.src == 0
                    && syscall_registry.lookup_by_key(insn.imm as u32).is_none() =>
                    {
                        return Err(VerifierError::InvalidSyscall(insn.imm as u32));
                    }
                ebpf::CALL_IMM if insn.src == 1 && insn.imm == -1 => {
                    return Err(VerifierError::InvalidFunction(insn_ptr));
                }

                _ => (),
            }

            insn_ptr += 1;
        }

        Ok(())
    }
}