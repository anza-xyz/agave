#![cfg(target_os = "linux")]

pub mod dir_remover;
pub mod file_creator;
pub mod memory;
pub mod sequential_file_reader;

// Based on Linux <uapi/linux/ioprio.h>
const IO_PRIO_CLASS_SHIFT: u16 = 13;
const IO_PRIO_CLASS_BE: u16 = 2;
const IO_PRIO_LEVEL_HIGHEST: u16 = 0;
const IO_PRIO_BE_HIGHEST: u16 = IO_PRIO_CLASS_BE << IO_PRIO_CLASS_SHIFT | IO_PRIO_LEVEL_HIGHEST;
