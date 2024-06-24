#![allow(clippy::arithmetic_side_effects)]
// Derived from uBPF <https://github.com/iovisor/ubpf>
// Copyright 2015 Big Switch Networks, Inc
//      (uBPF: VM architecture, parts of the interpreter, originally in C)
// Copyright 2016 6WIND S.A. <quentin.monnet@6wind.com>
//      (Translation to Rust, MetaBuff/multiple classes addition, hashmaps for syscalls)
// Copyright 2020 Solana Maintainers <maintainers@solana.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Virtual machine for eBPF programs.

use crate::{
    ebpf,
    elf::Executable,
    error::{EbpfError, ProgramResult},
    interpreter::Interpreter,
    memory_region::MemoryMapping,
    program::{BuiltinFunction, BuiltinProgram, FunctionRegistry, SBPFVersion},
    static_analysis::{Analysis, TraceLogEntry},
};
use rand::Rng;
use std::{collections::BTreeMap, fmt::Debug, sync::Arc};

/// Shift the RUNTIME_ENVIRONMENT_KEY by this many bits to the LSB
///
/// 3 bits for 8 Byte alignment, and 1 bit to have encoding space for the RuntimeEnvironment.
const PROGRAM_ENVIRONMENT_KEY_SHIFT: u32 = 4;
static RUNTIME_ENVIRONMENT_KEY: std::sync::OnceLock<i32> = std::sync::OnceLock::<i32>::new();

/// Returns (and if not done before generates) the encryption key for the VM pointer
pub fn get_runtime_environment_key() -> i32 {
    *RUNTIME_ENVIRONMENT_KEY
        .get_or_init(|| rand::thread_rng().gen::<i32>() >> PROGRAM_ENVIRONMENT_KEY_SHIFT)
}

/// VM configuration settings
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Maximum call depth
    pub max_call_depth: usize,
    /// Size of a stack frame in bytes, must match the size specified in the LLVM BPF backend
    pub stack_frame_size: usize,
    /// Enables the use of MemoryMapping and MemoryRegion for address translation
    pub enable_address_translation: bool,
    /// Enables gaps in VM address space between the stack frames
    pub enable_stack_frame_gaps: bool,
    /// Maximal pc distance after which a new instruction meter validation is emitted by the JIT
    pub instruction_meter_checkpoint_distance: usize,
    /// Enable instruction meter and limiting
    pub enable_instruction_meter: bool,
    /// Enable instruction tracing
    pub enable_instruction_tracing: bool,
    /// Enable dynamic string allocation for labels
    pub enable_symbol_and_section_labels: bool,
    /// Reject ELF files containing issues that the verifier did not catch before (up to v0.2.21)
    pub reject_broken_elfs: bool,
    /// Ratio of native host instructions per random no-op in JIT (0 = OFF)
    pub noop_instruction_rate: u32,
    /// Enable disinfection of immediate values and offsets provided by the user in JIT
    pub sanitize_user_provided_values: bool,
    /// Throw ElfError::SymbolHashCollision when a BPF function collides with a registered syscall
    pub external_internal_function_hash_collision: bool,
    /// Have the verifier reject "callx r10"
    pub reject_callx_r10: bool,
    /// Avoid copying read only sections when possible
    pub optimize_rodata: bool,
    /// Use the new ELF parser
    pub new_elf_parser: bool,
    /// Use aligned memory mapping
    pub aligned_memory_mapping: bool,
    /// Allow ExecutableCapability::V1
    pub enable_sbpf_v1: bool,
    /// Allow ExecutableCapability::V2
    pub enable_sbpf_v2: bool,
}

impl Config {
    /// Returns the size of the stack memory region
    pub fn stack_size(&self) -> usize {
        self.stack_frame_size * self.max_call_depth
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_call_depth: 64,
            stack_frame_size: 4_096,
            enable_address_translation: true,
            enable_stack_frame_gaps: true,
            instruction_meter_checkpoint_distance: 10000,
            enable_instruction_meter: true,
            enable_instruction_tracing: false,
            enable_symbol_and_section_labels: false,
            reject_broken_elfs: false,
            noop_instruction_rate: 256,
            sanitize_user_provided_values: true,
            external_internal_function_hash_collision: true,
            reject_callx_r10: true,
            optimize_rodata: true,
            new_elf_parser: true,
            aligned_memory_mapping: true,
            enable_sbpf_v1: true,
            enable_sbpf_v2: true,
        }
    }
}

/// Static constructors for Executable
impl<C: ContextObject> Executable<C> {
    /// Creates an executable from an ELF file
    pub fn from_elf(elf_bytes: &[u8], loader: Arc<BuiltinProgram<C>>) -> Result<Self, EbpfError> {
        let executable = Executable::load(elf_bytes, loader)?;
        Ok(executable)
    }
    /// Creates an executable from machine code
    pub fn from_text_bytes(
        text_bytes: &[u8],
        loader: Arc<BuiltinProgram<C>>,
        sbpf_version: SBPFVersion,
        function_registry: FunctionRegistry<usize>,
    ) -> Result<Self, EbpfError> {
        Executable::new_from_text_bytes(text_bytes, loader, sbpf_version, function_registry)
            .map_err(EbpfError::ElfError)
    }
}

/// Runtime context
pub trait ContextObject {
    /// Called for every instruction executed when tracing is enabled
    fn trace(&mut self, state: [u64; 12]);
    /// Consume instructions from meter
    fn consume(&mut self, amount: u64);
    /// Get the number of remaining instructions allowed
    fn get_remaining(&self) -> u64;
}

/// Simple instruction meter for testing
#[derive(Debug, Clone, Default)]
pub struct TestContextObject {
    /// Contains the register state at every instruction in order of execution
    pub trace_log: Vec<TraceLogEntry>,
    /// Maximal amount of instructions which still can be executed
    pub remaining: u64,
}

impl ContextObject for TestContextObject {
    fn trace(&mut self, state: [u64; 12]) {
        self.trace_log.push(state);
    }

    fn consume(&mut self, amount: u64) {
        self.remaining = self.remaining.saturating_sub(amount);
    }

    fn get_remaining(&self) -> u64 {
        self.remaining
    }
}

impl TestContextObject {
    /// Initialize with instruction meter
    pub fn new(remaining: u64) -> Self {
        Self {
            trace_log: Vec::new(),
            remaining,
        }
    }

    /// Compares an interpreter trace and a JIT trace.
    ///
    /// The log of the JIT can be longer because it only validates the instruction meter at branches.
    pub fn compare_trace_log(interpreter: &Self, jit: &Self) -> bool {
        let interpreter = interpreter.trace_log.as_slice();
        let mut jit = jit.trace_log.as_slice();
        if jit.len() > interpreter.len() {
            jit = &jit[0..interpreter.len()];
        }
        interpreter == jit
    }
}

/// Statistic of taken branches (from a recorded trace)
pub struct DynamicAnalysis {
    /// Maximal edge counter value
    pub edge_counter_max: usize,
    /// src_node, dst_node, edge_counter
    pub edges: BTreeMap<usize, BTreeMap<usize, usize>>,
}

impl DynamicAnalysis {
    /// Accumulates a trace
    pub fn new(trace_log: &[[u64; 12]], analysis: &Analysis) -> Self {
        let mut result = Self {
            edge_counter_max: 0,
            edges: BTreeMap::new(),
        };
        let mut last_basic_block = usize::MAX;
        for traced_instruction in trace_log.iter() {
            let pc = traced_instruction[11] as usize;
            if analysis.cfg_nodes.contains_key(&pc) {
                let counter = result
                    .edges
                    .entry(last_basic_block)
                    .or_default()
                    .entry(pc)
                    .or_insert(0);
                *counter += 1;
                result.edge_counter_max = result.edge_counter_max.max(*counter);
                last_basic_block = pc;
            }
        }
        result
    }
}

/// A call frame used for function calls inside the Interpreter
#[derive(Clone, Default)]
pub struct CallFrame {
    /// The caller saved registers
    pub caller_saved_registers: [u64; ebpf::SCRATCH_REGS],
    /// The callers frame pointer
    pub frame_pointer: u64,
    /// The target_pc of the exit instruction which returns back to the caller
    pub target_pc: u64,
}

/// A virtual machine to run eBPF programs.
///
/// # Examples
///
/// ```
/// use solana_rbpf::{
///     aligned_memory::AlignedMemory,
///     ebpf,
///     elf::Executable,
///     memory_region::{MemoryMapping, MemoryRegion},
///     program::{BuiltinProgram, FunctionRegistry, SBPFVersion},
///     verifier::RequisiteVerifier,
///     vm::{Config, EbpfVm, TestContextObject},
/// };
///
/// let prog = &[
///     0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00  // exit
/// ];
/// let mem = &mut [
///     0xaa, 0xbb, 0x11, 0x22, 0xcc, 0xdd
/// ];
///
/// let loader = std::sync::Arc::new(BuiltinProgram::new_mock());
/// let function_registry = FunctionRegistry::default();
/// let mut executable = Executable::<TestContextObject>::from_text_bytes(prog, loader.clone(), SBPFVersion::V2, function_registry).unwrap();
/// executable.verify::<RequisiteVerifier>().unwrap();
/// let mut context_object = TestContextObject::new(1);
/// let sbpf_version = executable.get_sbpf_version();
///
/// let mut stack = AlignedMemory::<{ebpf::HOST_ALIGN}>::zero_filled(executable.get_config().stack_size());
/// let stack_len = stack.len();
/// let mut heap = AlignedMemory::<{ebpf::HOST_ALIGN}>::with_capacity(0);
///
/// let regions: Vec<MemoryRegion> = vec![
///     executable.get_ro_region(),
///     MemoryRegion::new_writable(
///         stack.as_slice_mut(),
///         ebpf::MM_STACK_START,
///     ),
///     MemoryRegion::new_writable(heap.as_slice_mut(), ebpf::MM_HEAP_START),
///     MemoryRegion::new_writable(mem, ebpf::MM_INPUT_START),
/// ];
///
/// let memory_mapping = MemoryMapping::new(regions, executable.get_config(), sbpf_version).unwrap();
///
/// let mut vm = EbpfVm::new(loader, sbpf_version, &mut context_object, memory_mapping, stack_len);
///
/// let (instruction_count, result) = vm.execute_program(&executable, true);
/// assert_eq!(instruction_count, 1);
/// assert_eq!(result.unwrap(), 0);
/// ```
#[repr(C)]
pub struct EbpfVm<'a, C: ContextObject> {
    /// Needed to exit from the guest back into the host
    pub host_stack_pointer: *mut u64,
    /// The current call depth.
    ///
    /// Incremented on calls and decremented on exits. It's used to enforce
    /// config.max_call_depth and to know when to terminate execution.
    pub call_depth: u64,
    /// Guest stack pointer (r11).
    ///
    /// The stack pointer isn't exposed as an actual register. Only sub and add
    /// instructions (typically generated by the LLVM backend) are allowed to
    /// access it when sbpf_version.dynamic_stack_frames()=true. Its value is only
    /// stored here and therefore the register is not tracked in REGISTER_MAP.
    pub stack_pointer: u64,
    /// Pointer to ContextObject
    pub context_object_pointer: &'a mut C,
    /// Last return value of instruction_meter.get_remaining()
    pub previous_instruction_meter: u64,
    /// Outstanding value to instruction_meter.consume()
    pub due_insn_count: u64,
    /// CPU cycles accumulated by the stop watch
    pub stopwatch_numerator: u64,
    /// Number of times the stop watch was used
    pub stopwatch_denominator: u64,
    /// Registers inlined
    pub registers: [u64; 12],
    /// ProgramResult inlined
    pub program_result: ProgramResult,
    /// MemoryMapping inlined
    pub memory_mapping: MemoryMapping<'a>,
    /// Stack of CallFrames used by the Interpreter
    pub call_frames: Vec<CallFrame>,
    /// Loader built-in program
    pub loader: Arc<BuiltinProgram<C>>,
    /// TCP port for the debugger interface
    #[cfg(feature = "debugger")]
    pub debug_port: Option<u16>,
}

impl<'a, C: ContextObject> EbpfVm<'a, C> {
    /// Creates a new virtual machine instance.
    pub fn new(
        loader: Arc<BuiltinProgram<C>>,
        sbpf_version: &SBPFVersion,
        context_object: &'a mut C,
        mut memory_mapping: MemoryMapping<'a>,
        stack_len: usize,
    ) -> Self {
        let config = loader.get_config();
        let stack_pointer =
            ebpf::MM_STACK_START.saturating_add(if sbpf_version.dynamic_stack_frames() {
                // the stack is fully descending, frames start as empty and change size anytime r11 is modified
                stack_len
            } else {
                // within a frame the stack grows down, but frames are ascending
                config.stack_frame_size
            } as u64);
        if !config.enable_address_translation {
            memory_mapping = MemoryMapping::new_identity();
        }
        EbpfVm {
            host_stack_pointer: std::ptr::null_mut(),
            call_depth: 0,
            stack_pointer,
            context_object_pointer: context_object,
            previous_instruction_meter: 0,
            due_insn_count: 0,
            stopwatch_numerator: 0,
            stopwatch_denominator: 0,
            registers: [0u64; 12],
            program_result: ProgramResult::Ok(0),
            memory_mapping,
            call_frames: vec![CallFrame::default(); config.max_call_depth],
            loader,
            #[cfg(feature = "debugger")]
            debug_port: None,
        }
    }

    /// Execute the program
    ///
    /// If interpreted = `false` then the JIT compiled executable is used.
    pub fn execute_program(
        &mut self,
        executable: &Executable<C>,
        interpreted: bool,
    ) -> (u64, ProgramResult) {
        debug_assert!(Arc::ptr_eq(&self.loader, executable.get_loader()));
        // R1 points to beginning of input memory, R10 to the stack of the first frame, R11 is the pc (hidden)
        self.registers[1] = ebpf::MM_INPUT_START;
        self.registers[ebpf::FRAME_PTR_REG] = self.stack_pointer;
        self.registers[11] = executable.get_entrypoint_instruction_offset() as u64;
        let config = executable.get_config();
        let initial_insn_count = if config.enable_instruction_meter {
            self.context_object_pointer.get_remaining()
        } else {
            0
        };
        self.previous_instruction_meter = initial_insn_count;
        self.due_insn_count = 0;
        self.program_result = ProgramResult::Ok(0);
        if interpreted {
            #[cfg(feature = "debugger")]
            let debug_port = self.debug_port.clone();
            let mut interpreter = Interpreter::new(self, executable, self.registers);
            #[cfg(feature = "debugger")]
            if let Some(debug_port) = debug_port {
                crate::debugger::execute(&mut interpreter, debug_port);
            } else {
                while interpreter.step() {}
            }
            #[cfg(not(feature = "debugger"))]
            while interpreter.step() {}
        } else {
            #[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
            {
                let compiled_program = match executable
                    .get_compiled_program()
                    .ok_or_else(|| EbpfError::JitNotCompiled)
                {
                    Ok(compiled_program) => compiled_program,
                    Err(error) => return (0, ProgramResult::Err(error)),
                };
                compiled_program.invoke(config, self, self.registers);
            }
            #[cfg(not(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64")))]
            {
                return (0, ProgramResult::Err(EbpfError::JitNotCompiled));
            }
        };
        let instruction_count = if config.enable_instruction_meter {
            self.context_object_pointer.consume(self.due_insn_count);
            initial_insn_count.saturating_sub(self.context_object_pointer.get_remaining())
        } else {
            0
        };
        let mut result = ProgramResult::Ok(0);
        std::mem::swap(&mut result, &mut self.program_result);
        (instruction_count, result)
    }

    /// Invokes a built-in function
    pub fn invoke_function(&mut self, function: BuiltinFunction<C>) {
        function(
            unsafe {
                std::ptr::addr_of_mut!(*self)
                    .cast::<u64>()
                    .offset(get_runtime_environment_key() as isize)
                    .cast::<Self>()
            },
            self.registers[1],
            self.registers[2],
            self.registers[3],
            self.registers[4],
            self.registers[5],
        );
    }
}
