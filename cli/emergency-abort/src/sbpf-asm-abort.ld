PHDRS
{
  text    PT_LOAD    ; # Contains our executable code and constants
}

SECTIONS
{
  . = SIZEOF_HEADERS;
  .text : { *(.text) }
  .s : { *(.shstrtab) } # rename to save bytes
  /DISCARD/ : {
    *(.eh_frame*) # Exception handling frame information
    *(.gnu.hash*) # GNU-style hash tables
    *(.hash*)     # Any other type of hash tables
    *(.comment)   # Comments
    *(.symtab)    # Program symbol table
    *(.strtab)    # Program string table
    *(.dynstr)    # Dynamic linking symb
    *(.dynamic)   # Dynamic linking info
    *(.dynsym)    # Dynamic linking symbol table
  }
}

ENTRY (e) # Symbol name of our entrypoint 