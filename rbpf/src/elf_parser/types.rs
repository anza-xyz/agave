#![allow(missing_docs)]

pub type Elf64Half = u16;
pub type Elf64Word = u32;
pub type Elf64Xword = u64;
pub type Elf64Addr = u64;
pub type Elf64Off = u64;
pub type Elf64Section = u16;

#[derive(Debug, Clone)]
#[repr(C)]
pub struct ElfIdent {
    pub ei_mag: [u8; 4],
    pub ei_class: u8,
    pub ei_data: u8,
    pub ei_version: u8,
    pub ei_osabi: u8,
    pub ei_abiversion: u8,
    pub ei_pad: [u8; 7],
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct Elf64Ehdr {
    pub e_ident: ElfIdent,
    pub e_type: Elf64Half,
    pub e_machine: Elf64Half,
    pub e_version: Elf64Word,
    pub e_entry: Elf64Addr,
    pub e_phoff: Elf64Off,
    pub e_shoff: Elf64Off,
    pub e_flags: Elf64Word,
    pub e_ehsize: Elf64Half,
    pub e_phentsize: Elf64Half,
    pub e_phnum: Elf64Half,
    pub e_shentsize: Elf64Half,
    pub e_shnum: Elf64Half,
    pub e_shstrndx: Elf64Half,
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct Elf64Phdr {
    pub p_type: Elf64Word,
    pub p_flags: Elf64Word,
    pub p_offset: Elf64Off,
    pub p_vaddr: Elf64Addr,
    pub p_paddr: Elf64Addr,
    pub p_filesz: Elf64Xword,
    pub p_memsz: Elf64Xword,
    pub p_align: Elf64Xword,
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct Elf64Shdr {
    pub sh_name: Elf64Word,
    pub sh_type: Elf64Word,
    pub sh_flags: Elf64Xword,
    pub sh_addr: Elf64Addr,
    pub sh_offset: Elf64Off,
    pub sh_size: Elf64Xword,
    pub sh_link: Elf64Word,
    pub sh_info: Elf64Word,
    pub sh_addralign: Elf64Xword,
    pub sh_entsize: Elf64Xword,
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct Elf64Sym {
    pub st_name: Elf64Word,
    pub st_info: u8,
    pub st_other: u8,
    pub st_shndx: Elf64Section,
    pub st_value: Elf64Addr,
    pub st_size: Elf64Xword,
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct Elf64Dyn {
    pub d_tag: Elf64Xword,
    pub d_val: Elf64Xword,
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct Elf64Rel {
    pub r_offset: Elf64Addr,
    pub r_info: Elf64Xword,
}
