//! Internal ELF parser abstraction.
use std::{borrow::Cow, convert::TryInto, iter, ops::Range, slice};

use goblin::{
    elf::{Elf, Header, ProgramHeader, Reloc, SectionHeader, Sym},
    elf64::{
        header::{EI_ABIVERSION, EI_CLASS, EI_DATA, EI_OSABI, EI_VERSION},
        reloc::RelocIterator,
        sym::SymIterator,
    },
    error::Error as GoblinError,
};

use crate::{
    elf::ElfError,
    elf_parser::{
        consts::{SHF_ALLOC, SHF_WRITE, SHT_NOBITS, STT_FUNC},
        types::{
            Elf64Addr, Elf64Ehdr, Elf64Off, Elf64Phdr, Elf64Rel, Elf64Shdr, Elf64Sym, Elf64Word,
            Elf64Xword, ElfIdent,
        },
        Elf64, ElfParserError,
    },
    error::EbpfError,
};

/// The common trait implemented by LegacyParser and NewParser.
///
/// This is an internal interface used to isolate the ELF parsing bits and to be
/// able to plug the old goblin parser or the new parser depending on config.
///
/// The interface is pretty straightforward. The associated types are the types
/// used to represent ELF data. Some return values are `Cow<T>` since goblin
/// returns some data by value, while the new parser always borrows from the
/// underlying file slice.
pub trait ElfParser<'a>: Sized {
    /// Program header type.
    type ProgramHeader: ElfProgramHeader + 'a;
    /// Iterator of program headers.
    type ProgramHeaders: Iterator<Item = &'a Self::ProgramHeader>;

    /// Section header type.
    type SectionHeader: ElfSectionHeader + 'a;
    /// Iterator of section headers
    type SectionHeaders: Iterator<Item = &'a Self::SectionHeader>;

    /// Symbol type.
    type Symbol: ElfSymbol + 'a;
    /// Iterator of symbols.
    type Symbols: Iterator<Item = Cow<'a, Self::Symbol>>;

    /// Relocation type.
    type Relocation: ElfRelocation + 'a;
    /// Iterator of relocations.
    type Relocations: Iterator<Item = Cow<'a, Self::Relocation>>;

    /// Parses the ELF data included in the buffer.
    fn parse(data: &'a [u8]) -> Result<Self, ElfError>;

    /// Returns the file header.
    fn header(&self) -> &Elf64Ehdr;

    /// Returns the program headers.
    fn program_headers(&'a self) -> Self::ProgramHeaders;

    /// Returns the section headers.
    fn section_headers(&'a self) -> Self::SectionHeaders;

    /// Returns the section with the given `name`.
    fn section(&self, name: &[u8]) -> Result<Self::SectionHeader, ElfError>;

    /// Returns the section name at the given `sh_name` offset.
    fn section_name(&self, sh_name: Elf64Word) -> Option<&[u8]>;

    /// Returns the symbols included in the symbol table.
    fn symbols(&'a self) -> Self::Symbols;

    /// Returns the symbol name at the given `st_name` offset.
    fn symbol_name(&self, st_name: Elf64Word) -> Option<&[u8]>;

    /// Returns the symbols included in the dynamic symbol table.
    fn dynamic_symbol(&self, index: Elf64Word) -> Option<Self::Symbol>;

    /// Returns the dynamic symbol name at the given `st_name` offset.
    fn dynamic_symbol_name(&self, st_name: Elf64Word) -> Option<&[u8]>;

    /// Returns the dynamic relocations.
    fn dynamic_relocations(&'a self) -> Self::Relocations;
}

/// ELF program header.
pub trait ElfProgramHeader {
    /// Returns the segment virtual address.
    fn p_vaddr(&self) -> Elf64Addr;

    /// Returns the segment size when loaded in memory.
    fn p_memsz(&self) -> Elf64Xword;

    /// Returns the segment file offset.
    fn p_offset(&self) -> Elf64Off;

    /// Returns the segment virtual address range.
    fn vm_range(&self) -> Range<Elf64Addr> {
        let addr = self.p_vaddr();
        addr..addr.saturating_add(self.p_memsz())
    }
}

/// ELF section header.
pub trait ElfSectionHeader {
    /// Returns the section name offset.
    fn sh_name(&self) -> Elf64Word;

    /// Returns the section virtual address.
    fn sh_addr(&self) -> Elf64Addr;

    /// Returns the section file offset.
    fn sh_offset(&self) -> Elf64Off;

    /// Returns the section size.
    fn sh_size(&self) -> Elf64Xword;

    /// Returns the section flags.
    fn sh_flags(&self) -> Elf64Xword;

    /// Returns the section type.
    fn sh_type(&self) -> Elf64Word;

    /// Returns whether the section is writable.
    fn is_writable(&self) -> bool {
        self.sh_flags() & (SHF_ALLOC | SHF_WRITE) == SHF_ALLOC | SHF_WRITE
    }

    /// Returns the byte range the section spans in the file.
    fn file_range(&self) -> Option<Range<usize>> {
        (self.sh_type() != SHT_NOBITS).then(|| {
            let offset = self.sh_offset() as usize;
            offset..offset.saturating_add(self.sh_size() as usize)
        })
    }

    /// Returns the virtual address range.
    fn vm_range(&self) -> Range<Elf64Addr> {
        let addr = self.sh_addr();
        addr..addr.saturating_add(self.sh_size())
    }
}

/// ELF symbol.
pub trait ElfSymbol: Clone {
    /// Returns the symbol name offset.
    fn st_name(&self) -> Elf64Word;

    /// Returns the symbol type and binding attributes.
    fn st_info(&self) -> u8;

    /// Returns the value associated with the symbol.
    fn st_value(&self) -> Elf64Addr;

    /// Returns whether the symbol is a function.
    fn is_function(&self) -> bool {
        (self.st_info() & 0xF) == STT_FUNC
    }
}

/// ELF relocation.
pub trait ElfRelocation: Clone {
    /// Returns the offset where to apply the relocation.
    fn r_offset(&self) -> Elf64Addr;

    /// Returns the relocation type.
    fn r_type(&self) -> Elf64Word;

    /// Returns the symbol index.
    fn r_sym(&self) -> Elf64Word;
}

/// The Goblin based ELF parser.
pub struct GoblinParser<'a> {
    elf: Elf<'a>,
    header: Elf64Ehdr,
}

impl<'a> ElfParser<'a> for GoblinParser<'a> {
    type ProgramHeader = ProgramHeader;
    type ProgramHeaders = slice::Iter<'a, ProgramHeader>;

    type SectionHeader = SectionHeader;
    type SectionHeaders = slice::Iter<'a, SectionHeader>;

    type Symbol = Sym;
    type Symbols = iter::Map<SymIterator<'a>, fn(Self::Symbol) -> Cow<'a, Self::Symbol>>;

    type Relocation = Reloc;
    type Relocations =
        iter::Map<RelocIterator<'a>, fn(Self::Relocation) -> Cow<'a, Self::Relocation>>;

    fn parse(data: &'a [u8]) -> Result<GoblinParser<'a>, ElfError> {
        let elf = Elf::parse(data)?;
        Ok(Self {
            header: elf.header.into(),
            elf,
        })
    }

    fn header(&self) -> &Elf64Ehdr {
        &self.header
    }

    fn program_headers(&'a self) -> Self::ProgramHeaders {
        self.elf.program_headers.iter()
    }

    fn section_headers(&'a self) -> Self::SectionHeaders {
        self.elf.section_headers.iter()
    }

    fn section(&self, name: &[u8]) -> Result<Self::SectionHeader, ElfError> {
        match self.elf.section_headers.iter().find(|section_header| {
            if let Some(this_name) = self.section_name(section_header.sh_name as Elf64Word) {
                return this_name == name;
            }
            false
        }) {
            Some(section) => Ok(section.clone()),
            None => Err(ElfError::SectionNotFound(
                std::str::from_utf8(name)
                    .unwrap_or("UTF-8 error")
                    .to_string(),
            )),
        }
    }

    fn section_name(&self, sh_name: Elf64Word) -> Option<&[u8]> {
        self.elf
            .shdr_strtab
            .get_at(sh_name as usize)
            .map(|name| name.as_bytes())
    }

    fn symbols(&'a self) -> Self::Symbols {
        self.elf.syms.iter().map(Cow::Owned)
    }

    fn symbol_name(&self, st_name: Elf64Word) -> Option<&[u8]> {
        self.elf
            .strtab
            .get_at(st_name as usize)
            .map(|name| name.as_bytes())
    }

    fn dynamic_symbol(&self, index: Elf64Word) -> Option<Self::Symbol> {
        self.elf.dynsyms.get(index as usize)
    }

    fn dynamic_symbol_name(&self, st_name: Elf64Word) -> Option<&[u8]> {
        self.elf
            .dynstrtab
            .get_at(st_name as usize)
            .map(|name| name.as_bytes())
    }

    fn dynamic_relocations(&self) -> Self::Relocations {
        self.elf.dynrels.iter().map(Cow::Owned)
    }
}

impl From<Header> for Elf64Ehdr {
    fn from(h: Header) -> Self {
        Elf64Ehdr {
            e_ident: ElfIdent {
                ei_mag: h.e_ident[0..4].try_into().unwrap(),
                ei_class: h.e_ident[EI_CLASS],
                ei_data: h.e_ident[EI_DATA],
                ei_version: h.e_ident[EI_VERSION],
                ei_osabi: h.e_ident[EI_OSABI],
                ei_abiversion: h.e_ident[EI_ABIVERSION],
                ei_pad: [0u8; 7],
            },
            e_type: h.e_type,
            e_machine: h.e_machine,
            e_version: h.e_version,
            e_entry: h.e_entry,
            e_phoff: h.e_phoff,
            e_shoff: h.e_shoff,
            e_flags: h.e_flags,
            e_ehsize: h.e_ehsize,
            e_phentsize: h.e_phentsize,
            e_phnum: h.e_phnum,
            e_shentsize: h.e_shentsize,
            e_shnum: h.e_shnum,
            e_shstrndx: h.e_shstrndx,
        }
    }
}

impl ElfProgramHeader for ProgramHeader {
    fn p_vaddr(&self) -> Elf64Addr {
        self.p_vaddr
    }

    fn p_memsz(&self) -> Elf64Xword {
        self.p_memsz
    }

    fn p_offset(&self) -> Elf64Off {
        self.p_offset
    }
}

impl ElfSectionHeader for SectionHeader {
    fn sh_name(&self) -> Elf64Word {
        self.sh_name as _
    }

    fn sh_flags(&self) -> Elf64Xword {
        self.sh_flags
    }

    fn sh_addr(&self) -> Elf64Addr {
        self.sh_addr
    }

    fn sh_offset(&self) -> Elf64Off {
        self.sh_offset
    }

    fn sh_size(&self) -> Elf64Xword {
        self.sh_size
    }

    fn sh_type(&self) -> Elf64Word {
        self.sh_type
    }
}

impl ElfSymbol for Sym {
    fn st_name(&self) -> Elf64Word {
        self.st_name as _
    }

    fn st_info(&self) -> u8 {
        self.st_info
    }

    fn st_value(&self) -> Elf64Addr {
        self.st_value
    }
}

impl ElfRelocation for Reloc {
    fn r_offset(&self) -> Elf64Addr {
        self.r_offset
    }

    fn r_type(&self) -> Elf64Word {
        self.r_type
    }

    fn r_sym(&self) -> Elf64Word {
        self.r_sym as Elf64Word
    }
}

/// The new ELF parser.
#[derive(Debug)]
pub struct NewParser<'a> {
    elf: Elf64<'a>,
}

impl<'a> ElfParser<'a> for NewParser<'a> {
    type ProgramHeader = Elf64Phdr;
    type ProgramHeaders = slice::Iter<'a, Self::ProgramHeader>;

    type SectionHeader = Elf64Shdr;
    type SectionHeaders = slice::Iter<'a, Self::SectionHeader>;

    type Symbol = Elf64Sym;
    type Symbols =
        iter::Map<slice::Iter<'a, Self::Symbol>, fn(&'a Self::Symbol) -> Cow<'a, Self::Symbol>>;

    type Relocation = Elf64Rel;
    type Relocations = iter::Map<
        slice::Iter<'a, Self::Relocation>,
        fn(&'a Self::Relocation) -> Cow<'a, Self::Relocation>,
    >;

    fn parse(data: &'a [u8]) -> Result<NewParser<'a>, ElfError> {
        Ok(Self {
            elf: Elf64::parse(data)?,
        })
    }

    fn header(&self) -> &Elf64Ehdr {
        self.elf.file_header()
    }

    fn program_headers(&'a self) -> Self::ProgramHeaders {
        self.elf.program_header_table().iter()
    }

    fn section_headers(&'a self) -> Self::SectionHeaders {
        self.elf.section_header_table().iter()
    }

    fn section(&self, name: &[u8]) -> Result<Self::SectionHeader, ElfError> {
        for section_header in self.elf.section_header_table() {
            if self.elf.section_name(section_header.sh_name)? == name {
                return Ok(section_header.clone());
            }
        }

        Err(ElfError::SectionNotFound(
            std::str::from_utf8(name)
                .unwrap_or("UTF-8 error")
                .to_string(),
        ))
    }

    fn section_name(&self, sh_name: Elf64Word) -> Option<&[u8]> {
        self.elf.section_name(sh_name).ok()
    }

    fn symbols(&'a self) -> Self::Symbols {
        self.elf
            .symbol_table()
            .ok()
            .flatten()
            .unwrap_or(&[])
            .iter()
            .map(Cow::Borrowed)
    }

    fn symbol_name(&self, st_name: Elf64Word) -> Option<&[u8]> {
        self.elf.symbol_name(st_name).ok()
    }

    fn dynamic_symbol(&self, index: Elf64Word) -> Option<Self::Symbol> {
        self.elf
            .dynamic_symbol_table()
            .and_then(|table| table.get(index as usize).cloned())
    }

    fn dynamic_symbol_name(&self, st_name: Elf64Word) -> Option<&[u8]> {
        self.elf.dynamic_symbol_name(st_name).ok()
    }

    fn dynamic_relocations(&'a self) -> Self::Relocations {
        self.elf
            .dynamic_relocations_table()
            .unwrap_or(&[])
            .iter()
            .map(Cow::Borrowed)
    }
}

impl ElfProgramHeader for Elf64Phdr {
    fn p_vaddr(&self) -> Elf64Addr {
        self.p_vaddr
    }

    fn p_memsz(&self) -> Elf64Xword {
        self.p_memsz
    }

    fn p_offset(&self) -> Elf64Off {
        self.p_offset
    }
}

impl ElfSectionHeader for Elf64Shdr {
    fn sh_name(&self) -> Elf64Word {
        self.sh_name as _
    }

    fn sh_flags(&self) -> Elf64Xword {
        self.sh_flags
    }

    fn sh_addr(&self) -> Elf64Addr {
        self.sh_addr
    }

    fn sh_offset(&self) -> Elf64Off {
        self.sh_offset
    }

    fn sh_size(&self) -> Elf64Xword {
        self.sh_size
    }

    fn sh_type(&self) -> Elf64Word {
        self.sh_type
    }
}

impl ElfSymbol for Elf64Sym {
    fn st_name(&self) -> Elf64Word {
        self.st_name
    }

    fn st_info(&self) -> u8 {
        self.st_info
    }

    fn st_value(&self) -> Elf64Addr {
        self.st_value
    }
}

impl ElfRelocation for Elf64Rel {
    fn r_offset(&self) -> Elf64Addr {
        self.r_offset
    }

    fn r_type(&self) -> Elf64Word {
        (self.r_info & 0xFFFFFFFF) as Elf64Word
    }

    fn r_sym(&self) -> Elf64Word {
        self.r_info.checked_shr(32).unwrap_or(0) as Elf64Word
    }
}

impl From<ElfParserError> for ElfError {
    fn from(err: ElfParserError) -> Self {
        match err {
            ElfParserError::InvalidSectionHeader
            | ElfParserError::InvalidString
            | ElfParserError::StringTooLong(_, _)
            | ElfParserError::InvalidSize
            | ElfParserError::Overlap
            | ElfParserError::SectionNotInOrder
            | ElfParserError::NoSectionNameStringTable
            | ElfParserError::InvalidDynamicSectionTable
            | ElfParserError::InvalidRelocationTable
            | ElfParserError::InvalidAlignment
            | ElfParserError::NoStringTable
            | ElfParserError::NoDynamicStringTable
            | ElfParserError::InvalidFileHeader => ElfError::FailedToParse(err.to_string()),
            ElfParserError::InvalidProgramHeader => ElfError::InvalidProgramHeader,
            ElfParserError::OutOfBounds => ElfError::ValueOutOfBounds,
        }
    }
}

impl From<GoblinError> for ElfError {
    fn from(error: GoblinError) -> Self {
        match error {
            GoblinError::Malformed(string) => Self::FailedToParse(format!("malformed: {string}")),
            GoblinError::BadMagic(magic) => Self::FailedToParse(format!("bad magic: {magic:#x}")),
            GoblinError::Scroll(error) => Self::FailedToParse(format!("read-write: {error}")),
            GoblinError::IO(error) => Self::FailedToParse(format!("io: {error}")),
            GoblinError::BufferTooShort(n, error) => {
                Self::FailedToParse(format!("buffer too short {n} {error}"))
            }
            _ => Self::FailedToParse("cause unkown".to_string()),
        }
    }
}

impl From<GoblinError> for EbpfError {
    fn from(error: GoblinError) -> Self {
        ElfError::from(error).into()
    }
}
