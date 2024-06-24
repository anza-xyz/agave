//! Common interface for built-in and user supplied programs
use {
    crate::{
        ebpf,
        elf::ElfError,
        vm::{Config, ContextObject, EbpfVm},
    },
    std::collections::{btree_map::Entry, BTreeMap},
};

/// Defines a set of sbpf_version of an executable
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum SBPFVersion {
    /// The legacy format
    V1,
    /// The current format
    V2,
    /// The future format with BTF support
    V3,
}

impl SBPFVersion {
    /// Enable the little-endian byte swap instructions
    pub fn enable_le(&self) -> bool {
        self == &SBPFVersion::V1
    }

    /// Enable the negation instruction
    pub fn enable_neg(&self) -> bool {
        self == &SBPFVersion::V1
    }

    /// Swaps the reg and imm operands of the subtraction instruction
    pub fn swap_sub_reg_imm_operands(&self) -> bool {
        self != &SBPFVersion::V1
    }

    /// Enable the only two slots long instruction: LD_DW_IMM
    pub fn enable_lddw(&self) -> bool {
        self == &SBPFVersion::V1
    }

    /// Enable the BPF_PQR instruction class
    pub fn enable_pqr(&self) -> bool {
        self != &SBPFVersion::V1
    }

    /// Use src reg instead of imm in callx
    pub fn callx_uses_src_reg(&self) -> bool {
        self != &SBPFVersion::V1
    }

    /// Ensure that rodata sections don't exceed their maximum allowed size and
    /// overlap with the stack
    pub fn reject_rodata_stack_overlap(&self) -> bool {
        self != &SBPFVersion::V1
    }

    /// Allow sh_addr != sh_offset in elf sections. Used in V2 to align
    /// section vaddrs to MM_PROGRAM_START.
    pub fn enable_elf_vaddr(&self) -> bool {
        self != &SBPFVersion::V1
    }

    /// Use dynamic stack frame sizes
    pub fn dynamic_stack_frames(&self) -> bool {
        self != &SBPFVersion::V1
    }

    /// Support syscalls via pseudo calls (insn.src = 0)
    pub fn static_syscalls(&self) -> bool {
        self != &SBPFVersion::V1
    }
}

/// Holds the function symbols of an Executable
#[derive(Debug, PartialEq, Eq)]
pub struct FunctionRegistry<T> {
    pub(crate) map: BTreeMap<u32, (Vec<u8>, T)>,
}

impl<T> Default for FunctionRegistry<T> {
    fn default() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }
}

impl<T: Copy + PartialEq> FunctionRegistry<T> {
    /// Register a symbol with an explicit key
    pub fn register_function(
        &mut self,
        key: u32,
        name: impl Into<Vec<u8>>,
        value: T,
    ) -> Result<(), ElfError> {
        match self.map.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert((name.into(), value));
            }
            Entry::Occupied(entry) => {
                if entry.get().1 != value {
                    return Err(ElfError::SymbolHashCollision(key));
                }
            }
        }
        Ok(())
    }

    /// Register a symbol with an implicit key
    pub fn register_function_hashed(
        &mut self,
        name: impl Into<Vec<u8>>,
        value: T,
    ) -> Result<u32, ElfError> {
        let name = name.into();
        let key = ebpf::hash_symbol_name(name.as_slice());
        self.register_function(key, name, value)?;
        Ok(key)
    }

    /// Used for transitioning from SBPFv1 to SBPFv2
    pub(crate) fn register_function_hashed_legacy<C: ContextObject>(
        &mut self,
        loader: &BuiltinProgram<C>,
        hash_symbol_name: bool,
        name: impl Into<Vec<u8>>,
        value: T,
    ) -> Result<u32, ElfError>
    where
        usize: From<T>,
    {
        let name = name.into();
        let config = loader.get_config();
        let key = if hash_symbol_name {
            let hash = if name == b"entrypoint" {
                ebpf::hash_symbol_name(b"entrypoint")
            } else {
                ebpf::hash_symbol_name(&usize::from(value).to_le_bytes())
            };
            if config.external_internal_function_hash_collision
                && loader.get_function_registry().lookup_by_key(hash).is_some()
            {
                return Err(ElfError::SymbolHashCollision(hash));
            }
            hash
        } else {
            usize::from(value) as u32
        };
        self.register_function(
            key,
            if config.enable_symbol_and_section_labels || name == b"entrypoint" {
                name
            } else {
                Vec::default()
            },
            value,
        )?;
        Ok(key)
    }

    /// Unregister a symbol again
    pub fn unregister_function(&mut self, key: u32) {
        self.map.remove(&key);
    }

    /// Iterate over all keys
    pub fn keys(&self) -> impl Iterator<Item = u32> + '_ {
        self.map.keys().copied()
    }

    /// Iterate over all entries
    pub fn iter(&self) -> impl Iterator<Item = (u32, (&[u8], T))> + '_ {
        self.map
            .iter()
            .map(|(key, (name, value))| (*key, (name.as_slice(), *value)))
    }

    /// Get a function by its key
    pub fn lookup_by_key(&self, key: u32) -> Option<(&[u8], T)> {
        // String::from_utf8_lossy(function_name).as_str()
        self.map
            .get(&key)
            .map(|(function_name, value)| (function_name.as_slice(), *value))
    }

    /// Get a function by its name
    pub fn lookup_by_name(&self, name: &[u8]) -> Option<(&[u8], T)> {
        self.map
            .values()
            .find(|(function_name, _value)| function_name == name)
            .map(|(function_name, value)| (function_name.as_slice(), *value))
    }

    /// Calculate memory size
    pub fn mem_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.map.iter().fold(
            0,
            |state: usize, (_, (name, value))| {
                state.saturating_add(
                    std::mem::size_of_val(value).saturating_add(
                        std::mem::size_of_val(name).saturating_add(name.capacity()),
                    ),
                )
            },
        ))
    }
}

/// Syscall function without context
pub type BuiltinFunction<C> = fn(*mut EbpfVm<C>, u64, u64, u64, u64, u64);

/// Represents the interface to a fixed functionality program
#[derive(Eq)]
pub struct BuiltinProgram<C: ContextObject> {
    /// Holds the Config if this is a loader program
    config: Option<Box<Config>>,
    /// Function pointers by symbol
    functions: FunctionRegistry<BuiltinFunction<C>>,
}

impl<C: ContextObject> PartialEq for BuiltinProgram<C> {
    fn eq(&self, other: &Self) -> bool {
        self.config.eq(&other.config) && self.functions.eq(&other.functions)
    }
}

impl<C: ContextObject> BuiltinProgram<C> {
    /// Constructs a loader built-in program
    pub fn new_loader(config: Config, functions: FunctionRegistry<BuiltinFunction<C>>) -> Self {
        Self {
            config: Some(Box::new(config)),
            functions,
        }
    }

    /// Constructs a built-in program
    pub fn new_builtin(functions: FunctionRegistry<BuiltinFunction<C>>) -> Self {
        Self {
            config: None,
            functions,
        }
    }

    /// Constructs a mock loader built-in program
    pub fn new_mock() -> Self {
        Self {
            config: Some(Box::default()),
            functions: FunctionRegistry::default(),
        }
    }

    /// Get the configuration settings assuming this is a loader program
    pub fn get_config(&self) -> &Config {
        self.config.as_ref().unwrap()
    }

    /// Get the function registry
    pub fn get_function_registry(&self) -> &FunctionRegistry<BuiltinFunction<C>> {
        &self.functions
    }

    /// Calculate memory size
    pub fn mem_size(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(if self.config.is_some() {
                std::mem::size_of::<Config>()
            } else {
                0
            })
            .saturating_add(self.functions.mem_size())
    }
}

impl<C: ContextObject> std::fmt::Debug for BuiltinProgram<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        writeln!(f, "{:?}", unsafe {
            // `derive(Debug)` does not know that `C: ContextObject` does not need to implement `Debug`
            std::mem::transmute::<&FunctionRegistry<BuiltinFunction<C>>, &FunctionRegistry<usize>>(
                &self.functions,
            )
        })?;
        Ok(())
    }
}

/// Generates an adapter for a BuiltinFunction between the Rust and the VM interface
#[macro_export]
macro_rules! declare_builtin_function {
    ($(#[$attr:meta])* $name:ident $(<$($generic_ident:tt : $generic_type:tt),+>)?, fn rust(
        $vm:ident : &mut $ContextObject:ty,
        $arg_a:ident : u64,
        $arg_b:ident : u64,
        $arg_c:ident : u64,
        $arg_d:ident : u64,
        $arg_e:ident : u64,
        $memory_mapping:ident : &mut $MemoryMapping:ty,
    ) -> $Result:ty { $($rust:tt)* }) => {
        $(#[$attr])*
        pub struct $name {}
        impl $name {
            /// Rust interface
            pub fn rust $(<$($generic_ident : $generic_type),+>)? (
                $vm: &mut $ContextObject,
                $arg_a: u64,
                $arg_b: u64,
                $arg_c: u64,
                $arg_d: u64,
                $arg_e: u64,
                $memory_mapping: &mut $MemoryMapping,
            ) -> $Result {
                $($rust)*
            }
            /// VM interface
            #[allow(clippy::too_many_arguments)]
            pub fn vm $(<$($generic_ident : $generic_type),+>)? (
                $vm: *mut $crate::vm::EbpfVm<$ContextObject>,
                $arg_a: u64,
                $arg_b: u64,
                $arg_c: u64,
                $arg_d: u64,
                $arg_e: u64,
            ) {
                use $crate::vm::ContextObject;
                let vm = unsafe {
                    &mut *($vm.cast::<u64>().offset(-($crate::vm::get_runtime_environment_key() as isize)).cast::<$crate::vm::EbpfVm<$ContextObject>>())
                };
                let config = vm.loader.get_config();
                if config.enable_instruction_meter {
                    vm.context_object_pointer.consume(vm.previous_instruction_meter - vm.due_insn_count);
                }
                let converted_result: $crate::error::ProgramResult = Self::rust $(::<$($generic_ident),+>)?(
                    vm.context_object_pointer, $arg_a, $arg_b, $arg_c, $arg_d, $arg_e, &mut vm.memory_mapping,
                ).map_err(|err| $crate::error::EbpfError::SyscallError(err)).into();
                vm.program_result = converted_result;
                if config.enable_instruction_meter {
                    vm.previous_instruction_meter = vm.context_object_pointer.get_remaining();
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{syscalls, vm::TestContextObject};

    #[test]
    fn test_builtin_program_eq() {
        let mut function_registry_a =
            FunctionRegistry::<BuiltinFunction<TestContextObject>>::default();
        function_registry_a
            .register_function_hashed(*b"log", syscalls::SyscallString::vm)
            .unwrap();
        function_registry_a
            .register_function_hashed(*b"log_64", syscalls::SyscallU64::vm)
            .unwrap();
        let mut function_registry_b =
            FunctionRegistry::<BuiltinFunction<TestContextObject>>::default();
        function_registry_b
            .register_function_hashed(*b"log_64", syscalls::SyscallU64::vm)
            .unwrap();
        function_registry_b
            .register_function_hashed(*b"log", syscalls::SyscallString::vm)
            .unwrap();
        let mut function_registry_c =
            FunctionRegistry::<BuiltinFunction<TestContextObject>>::default();
        function_registry_c
            .register_function_hashed(*b"log_64", syscalls::SyscallU64::vm)
            .unwrap();
        let builtin_program_a = BuiltinProgram::new_loader(Config::default(), function_registry_a);
        let builtin_program_b = BuiltinProgram::new_loader(Config::default(), function_registry_b);
        assert_eq!(builtin_program_a, builtin_program_b);
        let builtin_program_c = BuiltinProgram::new_loader(Config::default(), function_registry_c);
        assert_ne!(builtin_program_a, builtin_program_c);
    }
}
