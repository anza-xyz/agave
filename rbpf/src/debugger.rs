//! Debugger for the virtual machines' interpreter.

use std::net::{TcpListener, TcpStream};

use gdbstub::common::Signal;
use gdbstub::conn::ConnectionExt;
use gdbstub::stub::{state_machine, GdbStub, SingleThreadStopReason};

use gdbstub::arch::lldb::{Encoding, Format, Generic, Register};
use gdbstub::arch::RegId;

use gdbstub::target;
use gdbstub::target::{Target, TargetError, TargetResult};

use core::convert::TryInto;

use bpf_arch::reg::id::BpfRegId;
use bpf_arch::reg::BpfRegs;
use bpf_arch::Bpf;
use gdbstub::target::ext::base::singlethread::{SingleThreadBase, SingleThreadResume};
use gdbstub::target::ext::lldb_register_info_override::{Callback, CallbackToken};
use gdbstub::target::ext::section_offsets::Offsets;

use crate::{
    ebpf,
    error::{EbpfError, ProgramResult},
    interpreter::{DebugState, Interpreter},
    memory_region::AccessType,
    vm::ContextObject,
};

type DynResult<T> = Result<T, Box<dyn std::error::Error>>;

fn wait_for_tcp(port: u16) -> DynResult<TcpStream> {
    let sockaddr = format!("127.0.0.1:{}", port);
    eprintln!("Waiting for a Debugger connection on {:?}...", sockaddr);

    let sock = TcpListener::bind(sockaddr)?;
    let (stream, addr) = sock.accept()?;
    eprintln!("Debugger connected from {}", addr);

    Ok(stream)
}

/// Connect to the debugger and hand over the control of the interpreter
pub fn execute<C: ContextObject>(interpreter: &mut Interpreter<C>, port: u16) {
    let connection: Box<dyn ConnectionExt<Error = std::io::Error>> =
        Box::new(wait_for_tcp(port).expect("Cannot connect to Debugger"));
    let mut dbg = GdbStub::new(connection)
        .run_state_machine(interpreter)
        .expect("Cannot start debugging state machine");
    loop {
        dbg = match dbg {
            state_machine::GdbStubStateMachine::Idle(mut dbg_inner) => {
                let byte = dbg_inner.borrow_conn().read().unwrap();
                dbg_inner.incoming_data(interpreter, byte).unwrap()
            }

            state_machine::GdbStubStateMachine::Disconnected(_dbg_inner) => {
                eprintln!("Client disconnected");
                break;
            }

            state_machine::GdbStubStateMachine::CtrlCInterrupt(dbg_inner) => dbg_inner
                .interrupt_handled(
                    interpreter,
                    Some(SingleThreadStopReason::Signal(Signal::SIGINT)),
                )
                .unwrap(),

            state_machine::GdbStubStateMachine::Running(mut dbg_inner) => {
                let conn = dbg_inner.borrow_conn();
                match interpreter.debug_state {
                    DebugState::Step => {
                        let mut stop_reason = if interpreter.step() {
                            SingleThreadStopReason::DoneStep
                        } else if let ProgramResult::Ok(result) = &interpreter.vm.program_result {
                            SingleThreadStopReason::Exited(*result as u8)
                        } else {
                            SingleThreadStopReason::Terminated(Signal::SIGSTOP)
                        };
                        if interpreter.breakpoints.contains(&interpreter.get_dbg_pc()) {
                            stop_reason = SingleThreadStopReason::SwBreak(());
                        }
                        dbg_inner.report_stop(interpreter, stop_reason).unwrap()
                    }
                    DebugState::Continue => loop {
                        if conn.peek().unwrap().is_some() {
                            let byte = dbg_inner.borrow_conn().read().unwrap();
                            break dbg_inner.incoming_data(interpreter, byte).unwrap();
                        }
                        if interpreter.step() {
                            if interpreter.breakpoints.contains(&interpreter.get_dbg_pc()) {
                                break dbg_inner
                                    .report_stop(interpreter, SingleThreadStopReason::SwBreak(()))
                                    .unwrap();
                            }
                        } else if let ProgramResult::Ok(result) = &interpreter.vm.program_result {
                            break dbg_inner
                                .report_stop(
                                    interpreter,
                                    SingleThreadStopReason::Exited(*result as u8),
                                )
                                .unwrap();
                        } else {
                            break dbg_inner
                                .report_stop(
                                    interpreter,
                                    SingleThreadStopReason::Terminated(Signal::SIGSTOP),
                                )
                                .unwrap();
                        }
                    },
                }
            }
        };
    }
}

impl<'a, 'b, C: ContextObject> Target for Interpreter<'a, 'b, C> {
    type Arch = Bpf;
    type Error = &'static str;

    #[inline(always)]
    fn base_ops(&mut self) -> target::ext::base::BaseOps<'_, Self::Arch, Self::Error> {
        target::ext::base::BaseOps::SingleThread(self)
    }

    #[inline(always)]
    fn support_breakpoints(
        &mut self,
    ) -> Option<target::ext::breakpoints::BreakpointsOps<'_, Self>> {
        Some(self)
    }

    #[inline(always)]
    fn support_section_offsets(
        &mut self,
    ) -> Option<target::ext::section_offsets::SectionOffsetsOps<'_, Self>> {
        Some(self)
    }

    #[inline(always)]
    fn support_lldb_register_info_override(
        &mut self,
    ) -> Option<target::ext::lldb_register_info_override::LldbRegisterInfoOverrideOps<'_, Self>>
    {
        Some(self)
    }
}

fn get_host_ptr<C: ContextObject>(
    interpreter: &mut Interpreter<C>,
    mut vm_addr: u64,
) -> Result<*mut u8, EbpfError> {
    if vm_addr < ebpf::MM_PROGRAM_START {
        vm_addr += ebpf::MM_PROGRAM_START;
    }
    match interpreter.vm.memory_mapping.map(
        AccessType::Load,
        vm_addr,
        std::mem::size_of::<u8>() as u64,
    ) {
        ProgramResult::Ok(host_addr) => Ok(host_addr as *mut u8),
        ProgramResult::Err(err) => Err(err),
    }
}

impl<'a, 'b, C: ContextObject> SingleThreadBase for Interpreter<'a, 'b, C> {
    fn read_registers(&mut self, regs: &mut BpfRegs) -> TargetResult<(), Self> {
        for i in 0..10 {
            regs.r[i] = self.reg[i];
        }
        regs.sp = self.reg[ebpf::FRAME_PTR_REG];
        regs.pc = self.get_dbg_pc();
        Ok(())
    }

    fn write_registers(&mut self, regs: &BpfRegs) -> TargetResult<(), Self> {
        for i in 0..10 {
            self.reg[i] = regs.r[i];
        }
        self.reg[ebpf::FRAME_PTR_REG] = regs.sp;
        self.reg[11] = regs.pc;
        Ok(())
    }

    #[inline(always)]
    fn support_single_register_access(
        &mut self,
    ) -> Option<target::ext::base::single_register_access::SingleRegisterAccessOps<'_, (), Self>>
    {
        Some(self)
    }

    fn read_addrs(&mut self, start_addr: u64, data: &mut [u8]) -> TargetResult<(), Self> {
        for (vm_addr, val) in (start_addr..).zip(data.iter_mut()) {
            let host_ptr = match get_host_ptr(self, vm_addr) {
                Ok(host_ptr) => host_ptr,
                // The debugger is sometimes requesting more data than we have access to, just skip these
                _ => continue,
            };
            *val = unsafe { *host_ptr as u8 };
        }
        Ok(())
    }

    fn write_addrs(&mut self, start_addr: u64, data: &[u8]) -> TargetResult<(), Self> {
        for (_addr, _val) in (start_addr..).zip(data.iter().copied()) {
            eprintln!("Memory write not supported");
        }
        Ok(())
    }

    #[inline(always)]
    fn support_resume(
        &mut self,
    ) -> Option<target::ext::base::singlethread::SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

impl<'a, 'b, C: ContextObject> target::ext::base::single_register_access::SingleRegisterAccess<()>
    for Interpreter<'a, 'b, C>
{
    fn read_register(
        &mut self,
        _tid: (),
        reg_id: BpfRegId,
        buf: &mut [u8],
    ) -> TargetResult<usize, Self> {
        match reg_id {
            BpfRegId::Gpr(i) => {
                let r = self.reg[i as usize];
                buf.copy_from_slice(&r.to_le_bytes());
            }
            BpfRegId::Sp => buf.copy_from_slice(&self.reg[ebpf::FRAME_PTR_REG].to_le_bytes()),
            BpfRegId::Pc => buf.copy_from_slice(&self.get_dbg_pc().to_le_bytes()),
            BpfRegId::InstructionCountRemaining => {
                buf.copy_from_slice(&self.vm.context_object_pointer.get_remaining().to_le_bytes())
            }
        }
        Ok(buf.len())
    }

    fn write_register(&mut self, _tid: (), reg_id: BpfRegId, val: &[u8]) -> TargetResult<(), Self> {
        let r = u64::from_le_bytes(
            val.try_into()
                .map_err(|_| TargetError::Fatal("invalid data"))?,
        );

        match reg_id {
            BpfRegId::Gpr(i) => self.reg[i as usize] = r,
            BpfRegId::Sp => self.reg[ebpf::FRAME_PTR_REG] = r,
            BpfRegId::Pc => self.reg[11] = r,
            BpfRegId::InstructionCountRemaining => (),
        }
        Ok(())
    }
}

impl<'a, 'b, C: ContextObject> SingleThreadResume for Interpreter<'a, 'b, C> {
    fn resume(&mut self, signal: Option<Signal>) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("no support for continuing with signal");
        }

        self.debug_state = DebugState::Continue;

        Ok(())
    }

    #[inline(always)]
    fn support_single_step(
        &mut self,
    ) -> Option<target::ext::base::singlethread::SingleThreadSingleStepOps<'_, Self>> {
        Some(self)
    }
}

impl<'a, 'b, C: ContextObject> target::ext::base::singlethread::SingleThreadSingleStep
    for Interpreter<'a, 'b, C>
{
    fn step(&mut self, signal: Option<Signal>) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("no support for stepping with signal");
        }

        self.debug_state = DebugState::Step;

        Ok(())
    }
}

impl<'a, 'b, C: ContextObject> target::ext::section_offsets::SectionOffsets
    for Interpreter<'a, 'b, C>
{
    fn get_section_offsets(&mut self) -> Result<Offsets<u64>, Self::Error> {
        Ok(Offsets::Sections {
            text: 0,
            data: 0,
            bss: None,
        })
    }
}

impl<'a, 'b, C: ContextObject> target::ext::breakpoints::Breakpoints for Interpreter<'a, 'b, C> {
    #[inline(always)]
    fn support_sw_breakpoint(
        &mut self,
    ) -> Option<target::ext::breakpoints::SwBreakpointOps<'_, Self>> {
        Some(self)
    }
}

impl<'a, 'b, C: ContextObject> target::ext::breakpoints::SwBreakpoint for Interpreter<'a, 'b, C> {
    fn add_sw_breakpoint(
        &mut self,
        addr: u64,
        _kind: bpf_arch::BpfBreakpointKind,
    ) -> TargetResult<bool, Self> {
        self.breakpoints.push(addr);

        Ok(true)
    }

    fn remove_sw_breakpoint(
        &mut self,
        addr: u64,
        _kind: bpf_arch::BpfBreakpointKind,
    ) -> TargetResult<bool, Self> {
        match self.breakpoints.iter().position(|x| *x == addr) {
            None => return Ok(false),
            Some(pos) => self.breakpoints.remove(pos),
        };

        Ok(true)
    }
}

impl<'a, 'b, C: ContextObject> target::ext::lldb_register_info_override::LldbRegisterInfoOverride
    for Interpreter<'a, 'b, C>
{
    fn lldb_register_info<'c>(
        &mut self,
        reg_id: usize,
        reg_info: Callback<'c>,
    ) -> Result<CallbackToken<'c>, Self::Error> {
        match BpfRegId::from_raw_id(reg_id) {
            Some((_, None)) | None => Ok(reg_info.done()),
            Some((r, Some(size))) => {
                let name: String = match r {
                    BpfRegId::Gpr(i) => match i {
                        0 => "r0",
                        1 => "r1",
                        2 => "r2",
                        3 => "r3",
                        4 => "r4",
                        5 => "r5",
                        6 => "r6",
                        7 => "r7",
                        8 => "r8",
                        9 => "r9",
                        _ => "unknown",
                    },
                    BpfRegId::Sp => "sp",
                    BpfRegId::Pc => "pc",
                    BpfRegId::InstructionCountRemaining => "remaining",
                }
                .into();
                let set = String::from("General Purpose Registers");
                let generic = match r {
                    BpfRegId::Sp => Some(Generic::Sp),
                    BpfRegId::Pc => Some(Generic::Pc),
                    _ => None,
                };
                let reg = Register {
                    name: &name,
                    alt_name: None,
                    bitsize: (usize::from(size)) * 8,
                    offset: reg_id * (usize::from(size)),
                    encoding: Encoding::Uint,
                    format: Format::Hex,
                    set: &set,
                    gcc: None,
                    dwarf: Some(reg_id),
                    generic,
                    container_regs: None,
                    invalidate_regs: None,
                };
                Ok(reg_info.write(reg))
            }
        }
    }
}

mod bpf_arch {
    use gdbstub::arch::{Arch, SingleStepGdbBehavior};

    /// BPF-specific breakpoint kinds.
    ///
    /// Extracted from the GDB source code [BPF Breakpoint Kinds](https://github.com/bminor/binutils-gdb/blob/9e0f6329352ab9c5e2f278181a3875918cff3b27/gdb/bpf-tdep.c#L205)
    #[derive(Debug)]
    pub enum BpfBreakpointKind {
        /// BPF breakpoint
        BpfBpKindBrkpt,
    }

    impl gdbstub::arch::BreakpointKind for BpfBreakpointKind {
        fn from_usize(kind: usize) -> Option<Self> {
            let kind = match kind {
                0 => BpfBreakpointKind::BpfBpKindBrkpt,
                _ => return None,
            };
            Some(kind)
        }
    }

    /// Implements `Arch` for BPF.
    pub enum Bpf {}

    #[allow(deprecated)]
    impl Arch for Bpf {
        type Usize = u64;
        type Registers = reg::BpfRegs;
        type RegId = reg::id::BpfRegId;
        type BreakpointKind = BpfBreakpointKind;

        #[inline(always)]
        fn single_step_gdb_behavior() -> SingleStepGdbBehavior {
            SingleStepGdbBehavior::Required
        }
    }

    pub mod reg {
        pub use bpf::BpfRegs;

        mod bpf {
            use core::convert::TryInto;

            use gdbstub::arch::Registers;

            /// BPF registers.
            ///
            /// Source: <https://github.com/bminor/binutils-gdb/blob/9e0f6329352ab9c5e2f278181a3875918cff3b27/gdb/bpf-tdep.c#L42>
            #[derive(Debug, Default, Clone, Eq, PartialEq)]
            pub struct BpfRegs {
                /// General purpose registers (R0-R9)
                pub r: [u64; 10],
                /// Stack pointer (R10)
                pub sp: u64,
                /// Program counter (R11)
                pub pc: u64,
            }

            impl Registers for BpfRegs {
                type ProgramCounter = u64;

                fn pc(&self) -> Self::ProgramCounter {
                    self.pc
                }

                fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
                    macro_rules! write_bytes {
                        ($bytes:expr) => {
                            for b in $bytes {
                                write_byte(Some(*b))
                            }
                        };
                    }

                    // Write GPRs
                    for reg in self.r.iter() {
                        write_bytes!(&reg.to_le_bytes());
                    }

                    // Write stack pointer register
                    write_bytes!(&self.sp.to_le_bytes());
                    // Write program counter register
                    write_bytes!(&self.pc.to_le_bytes());
                }

                fn gdb_deserialize(&mut self, mut bytes: &[u8]) -> Result<(), ()> {
                    // Ensure bytes contains enough data for all 12 registers
                    if bytes.len() < (12 * 8) {
                        return Err(());
                    }

                    let mut next_reg = || {
                        if bytes.len() < 8 {
                            Err(())
                        } else {
                            let (next, rest) = bytes.split_at(8);
                            bytes = rest;
                            Ok(u64::from_le_bytes(next.try_into().unwrap()))
                        }
                    };

                    // Read general purpose register
                    for reg in self.r.iter_mut() {
                        *reg = next_reg()?
                    }
                    self.sp = next_reg()?;
                    self.pc = next_reg()?;

                    if next_reg().is_ok() {
                        return Err(());
                    }

                    Ok(())
                }
            }
        }
        pub mod id {
            use core::num::NonZeroUsize;

            use gdbstub::arch::RegId;

            /// BPF register identifier.
            #[derive(Debug, Clone, Copy)]
            #[non_exhaustive]
            pub enum BpfRegId {
                /// General purpose registers (R0-R9)
                Gpr(u8),
                /// Stack Pointer (R10)
                Sp,
                /// Program Counter (R11)
                Pc,
                /// Instruction Counter (pseudo register)
                InstructionCountRemaining,
            }

            impl RegId for BpfRegId {
                fn from_raw_id(id: usize) -> Option<(BpfRegId, Option<NonZeroUsize>)> {
                    let reg = match id {
                        0..=9 => {
                            return Some((BpfRegId::Gpr(id as u8), Some(NonZeroUsize::new(8)?)))
                        }
                        10 => BpfRegId::Sp,
                        11 => BpfRegId::Pc,
                        12 => BpfRegId::InstructionCountRemaining,
                        _ => return None,
                    };
                    Some((reg, Some(NonZeroUsize::new(8)?)))
                }
            }
        }
    }
}
