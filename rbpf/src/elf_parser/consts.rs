#![allow(dead_code, missing_docs)]

use super::types::*;

pub const ELFMAG: [u8; 4] = [0x7F, 0x45, 0x4C, 0x46];

pub const ELFCLASSNONE: u8 = 0;
pub const ELFCLASS32: u8 = 1;
pub const ELFCLASS64: u8 = 2;

pub const ELFDATANONE: u8 = 0;
pub const ELFDATA2LSB: u8 = 1;
pub const ELFDATA2MSB: u8 = 2;

pub const EI_OSABI: u8 = 7;
pub const ELFOSABI_NONE: u8 = 0;

pub const EM_BPF: Elf64Half = 247;
pub const EM_SBPF: Elf64Half = 263;

pub const ET_NONE: Elf64Half = 0;
pub const ET_REL: Elf64Half = 1;
pub const ET_EXEC: Elf64Half = 2;
pub const ET_DYN: Elf64Half = 3;
pub const ET_CORE: Elf64Half = 4;

pub const EV_NONE: Elf64Word = 0;
pub const EV_CURRENT: Elf64Word = 1;

pub const PT_NULL: Elf64Word = 0;
pub const PT_LOAD: Elf64Word = 1;
pub const PT_DYNAMIC: Elf64Word = 2;
pub const PT_INTERP: Elf64Word = 3;
pub const PT_NOTE: Elf64Word = 4;
pub const PT_SHLIB: Elf64Word = 5;
pub const PT_PHDR: Elf64Word = 6;
pub const PT_TLS: Elf64Word = 7;
pub const PT_GNU_EH_FRAME: Elf64Word = 0x6474E550;
pub const PT_GNU_STACK: Elf64Word = 0x6474E551;

pub const PF_X: Elf64Word = 0x1;
pub const PF_W: Elf64Word = 0x2;
pub const PF_R: Elf64Word = 0x4;

pub const SHT_NULL: Elf64Word = 0;
pub const SHT_PROGBITS: Elf64Word = 1;
pub const SHT_SYMTAB: Elf64Word = 2;
pub const SHT_STRTAB: Elf64Word = 3;
pub const SHT_RELA: Elf64Word = 4;
pub const SHT_HASH: Elf64Word = 5;
pub const SHT_DYNAMIC: Elf64Word = 6;
pub const SHT_NOTE: Elf64Word = 7;
pub const SHT_NOBITS: Elf64Word = 8;
pub const SHT_REL: Elf64Word = 9;
pub const SHT_SHLIB: Elf64Word = 10;
pub const SHT_DYNSYM: Elf64Word = 11;
pub const SHT_INIT_ARRAY: Elf64Word = 14;
pub const SHT_FINI_ARRAY: Elf64Word = 15;
pub const SHT_PREINIT_ARRAY: Elf64Word = 16;
pub const SHT_GROUP: Elf64Word = 17;
pub const SHT_SYMTAB_SHNDX: Elf64Word = 18;

pub const SHF_WRITE: Elf64Xword = 0x1;
pub const SHF_ALLOC: Elf64Xword = 0x2;
pub const SHF_EXECINSTR: Elf64Xword = 0x4;
pub const SHF_MERGE: Elf64Xword = 0x10;
pub const SHF_STRINGS: Elf64Xword = 0x20;
pub const SHF_INFO_LINK: Elf64Xword = 0x40;
pub const SHF_LINK_ORDER: Elf64Xword = 0x80;
pub const SHF_OS_NONCONFORMING: Elf64Xword = 0x100;
pub const SHF_GROUP: Elf64Xword = 0x200;
pub const SHF_TLS: Elf64Xword = 0x400;

pub const SHN_UNDEF: Elf64Half = 0;

pub const DT_NULL: Elf64Xword = 0;
pub const DT_NEEDED: Elf64Xword = 1;
pub const DT_PLTRELSZ: Elf64Xword = 2;
pub const DT_PLTGOT: Elf64Xword = 3;
pub const DT_HASH: Elf64Xword = 4;
pub const DT_STRTAB: Elf64Xword = 5;
pub const DT_SYMTAB: Elf64Xword = 6;
pub const DT_RELA: Elf64Xword = 7;
pub const DT_RELASZ: Elf64Xword = 8;
pub const DT_RELAENT: Elf64Xword = 9;
pub const DT_STRSZ: Elf64Xword = 10;
pub const DT_SYMENT: Elf64Xword = 11;
pub const DT_INIT: Elf64Xword = 12;
pub const DT_FINI: Elf64Xword = 13;
pub const DT_SONAME: Elf64Xword = 14;
pub const DT_RPATH: Elf64Xword = 15;
pub const DT_SYMBOLIC: Elf64Xword = 16;
pub const DT_REL: Elf64Xword = 17;
pub const DT_RELSZ: Elf64Xword = 18;
pub const DT_RELENT: Elf64Xword = 19;
pub const DT_PLTREL: Elf64Xword = 20;
pub const DT_DEBUG: Elf64Xword = 21;
pub const DT_TEXTREL: Elf64Xword = 22;
pub const DT_JMPREL: Elf64Xword = 23;
pub const DT_BIND_NOW: Elf64Xword = 24;
pub const DT_INIT_ARRAY: Elf64Xword = 25;
pub const DT_FINI_ARRAY: Elf64Xword = 26;
pub const DT_INIT_ARRAYSZ: Elf64Xword = 27;
pub const DT_FINI_ARRAYSZ: Elf64Xword = 28;
pub const DT_RUNPATH: Elf64Xword = 29;
pub const DT_FLAGS: Elf64Xword = 30;
pub const DT_ENCODING: Elf64Xword = 32;
pub const DT_PREINIT_ARRAY: Elf64Xword = 32;
pub const DT_PREINIT_ARRAYSZ: Elf64Xword = 33;
pub const DT_SYMTAB_SHNDX: Elf64Xword = 34;
pub const DT_NUM: usize = 35;

pub const STT_NOTYPE: u8 = 0;
pub const STT_OBJECT: u8 = 1;
pub const STT_FUNC: u8 = 2;
pub const STT_SECTION: u8 = 3;
pub const STT_FILE: u8 = 4;
pub const STT_COMMON: u8 = 5;
pub const STT_TLS: u8 = 6;
pub const STT_NUM: u8 = 7;
pub const STT_LOOS: u8 = 10;
pub const STT_GNU_IFUNC: u8 = 10;
pub const STT_HIOS: u8 = 12;
pub const STT_LOPROC: u8 = 13;
pub const STT_HIPROC: u8 = 15;

pub const R_X86_64_NONE: u32 = 0;
pub const R_X86_64_64: u32 = 1;
pub const R_X86_64_PC32: u32 = 2;
pub const R_X86_64_GOT32: u32 = 3;
pub const R_X86_64_PLT32: u32 = 4;
pub const R_X86_64_COPY: u32 = 5;
pub const R_X86_64_GLOB_DAT: u32 = 6;
pub const R_X86_64_JUMP_SLOT: u32 = 7;
pub const R_X86_64_RELATIVE: u32 = 8;
pub const R_X86_64_GOTPCREL: u32 = 9;
pub const R_X86_64_32: u32 = 10;
pub const R_X86_64_32S: u32 = 11;
pub const R_X86_64_16: u32 = 12;
pub const R_X86_64_PC16: u32 = 13;
pub const R_X86_64_8: u32 = 14;
pub const R_X86_64_PC8: u32 = 15;
pub const R_X86_64_DTPMOD64: u32 = 16;
pub const R_X86_64_DTPOFF64: u32 = 17;
pub const R_X86_64_TPOFF64: u32 = 18;
pub const R_X86_64_TLSGD: u32 = 19;
pub const R_X86_64_TLSLD: u32 = 20;
pub const R_X86_64_DTPOFF32: u32 = 21;
pub const R_X86_64_GOTTPOFF: u32 = 22;
pub const R_X86_64_TPOFF32: u32 = 23;
pub const R_X86_64_PC64: u32 = 24;
pub const R_X86_64_GOTOFF64: u32 = 25;
pub const R_X86_64_GOTPC32: u32 = 26;
pub const R_X86_64_GOT64: u32 = 27;
pub const R_X86_64_GOTPCREL64: u32 = 28;
pub const R_X86_64_GOTPC64: u32 = 29;
pub const R_X86_64_GOTPLT64: u32 = 30;
pub const R_X86_64_PLTOFF64: u32 = 31;
pub const R_X86_64_SIZE32: u32 = 32;
pub const R_X86_64_SIZE64: u32 = 33;
pub const R_X86_64_GOTPC32_TLSDESC: u32 = 34;
pub const R_X86_64_TLSDESC_CALL: u32 = 35;
pub const R_X86_64_TLSDESC: u32 = 36;
pub const R_X86_64_IRELATIVE: u32 = 37;
pub const R_X86_64_RELATIVE64: u32 = 38;
pub const R_X86_64_GOTPCRELX: u32 = 41;
pub const R_X86_64_REX_GOTPCRELX: u32 = 42;
pub const R_X86_64_NUM: u32 = 43;
