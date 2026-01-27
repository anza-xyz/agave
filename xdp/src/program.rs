#![allow(clippy::arithmetic_side_effects)]

use {
    crate::device::NetworkDevice,
    agave_xdp_ebpf::{DecisionEvent, FirewallConfig, FirewallRule, DECISION_EVENT_SIZE},
    aya::{
        maps::{Array, MapData, RingBuf},
        programs::Xdp,
        Ebpf,
    },
    libc::{poll, pollfd, POLLERR, POLLIN},
    log::{error, info, trace, warn},
    std::{
        io::{Cursor, Write},
        os::fd::AsRawFd,
        thread,
    },
};

macro_rules! write_fields {
    ($w:expr, $($x:expr),*) => {
        $(
            $w.write_all(&$x.to_le_bytes())?;
        )*
    };
}

const SHT_NULL: u32 = 0;
// text section
const SHT_PROGBITS: u32 = 1;
// symbol table
const SHT_SYMTAB: u32 = 2;
// string table
const SHT_STRTAB: u32 = 3;

// flags required for the text section
const SHF_ALLOC: u64 = 1 << 1;
const SHF_EXECINSTR: u64 = 1 << 2;

// symbol visibility
const STB_GLOBAL: u8 = 1 << 4;
// symbol type
const STT_FUNC: u8 = 2;

// we just let all packets in
const XDP_PROG: &[u8] = &[
    0xb7, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // r0 = XDP_PASS
    0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
];

// the string table
const STRTAB: &[u8] = b"\0xdp\0.symtab\0.strtab\0agave_xdp\0";

pub fn load_xdp_program(
    dev: &NetworkDevice,
    firewall_config: Option<FirewallConfig>,
    firewall_rules: &[FirewallRule],
) -> Result<Ebpf, Box<dyn std::error::Error>> {
    let broken_frags = dev.driver()? == "i40e";
    let load_firewall = broken_frags || firewall_config.is_some();

    let mut firewall_config = firewall_config.unwrap_or_default();
    let mut ebpf = if load_firewall {
        info!("Loading the XDP program with firewall support");
        let ebpf = Ebpf::load(agave_xdp_ebpf::AGAVE_XDP_EBPF_PROGRAM)?;
        firewall_config.drop_frags = broken_frags;
        ebpf
    } else {
        info!("Loading the bypass XDP program");
        Ebpf::load(&generate_xdp_elf())?
    };

    let p: &mut Xdp = ebpf.program_mut("agave_xdp").unwrap().try_into().unwrap();

    p.load()?;
    p.attach_to_if_index(dev.if_index(), aya::programs::xdp::XdpFlags::DRV_MODE)?;

    if load_firewall {
        let mut config_map = Array::try_from(
            ebpf.map_mut("FIREWALL_CONFIG")
                .expect("Must have loaded the correct program"),
        )?;
        config_map.set(0, firewall_config, 0)?;
        let mut rules_map: Array<_, FirewallRule> = Array::try_from(
            ebpf.map_mut("FIREWALL_RULES")
                .expect("Must have loaded the correct program"),
        )?;
        for (index, rule) in firewall_rules.iter().enumerate() {
            rules_map.set(index as u32, *rule, 0)?;
        }
        info!("Firewall configured with {firewall_config:?}");
        let ringbuf = RingBuf::try_from(
            ebpf.take_map("RING_BUF")
                .expect("Must have loaded the correct program"),
        )?;
        thread::spawn(move || watch_ring(ringbuf));
    }
    Ok(ebpf)
}

fn watch_ring(mut ring: RingBuf<MapData>) {
    let mut fds = [pollfd {
        fd: ring.as_raw_fd(),
        events: POLLIN | POLLERR,
        revents: 0,
    }];

    loop {
        // Wait up to 100ms for events
        let ret = unsafe { poll(fds.as_mut_ptr(), fds.len() as u64, 1000) };
        if ret < 0 {
            error!("poll failed, terminating firewall monitor thread");
            break;
        }
        if ret == 0 {
            warn!("timeout, no events");
            continue;
        }
        // Drain everything currently in the ring
        loop {
            let item = ring.next();
            let Some(read) = item else {
                break;
            };
            assert_eq!(read.len(), DECISION_EVENT_SIZE, "Invalid event size");

            let ptr = read.as_ptr();
            let event =
                unsafe { std::ptr::read_unaligned::<DecisionEvent>(ptr as *const DecisionEvent) };

            trace!(
                "Firewall decision: {:?} for port {}",
                event.decision,
                event.dst_port,
            );
        }
    }
}

fn generate_xdp_elf() -> Vec<u8> {
    let mut buffer = vec![0u8; 4096];
    let mut cursor = Cursor::new(&mut buffer);

    // start after the header
    let xdp_off = 64;
    cursor.set_position(xdp_off);
    cursor.write_all(XDP_PROG).unwrap();
    let xdp_size = cursor.position() - xdp_off;

    // write the string table
    let strtab_off = cursor.position();
    cursor.write_all(STRTAB).unwrap();
    let strtab_size = cursor.position() - strtab_off;

    // write the symbol table
    let symtab_off = align_cursor(&mut cursor, 8);
    write_symbol(&mut cursor, 0, 0, 0, 0, 0, 0).unwrap();
    write_symbol(
        &mut cursor,
        21, // index
        0,
        XDP_PROG.len() as u64,
        STB_GLOBAL | STT_FUNC,
        0,
        1, // section index
    )
    .unwrap();
    let symtab_size = cursor.position() - symtab_off;

    // write the section headers
    let shdrs_off = align_cursor(&mut cursor, 8);
    write_section_headers(
        &mut cursor,
        xdp_off,
        xdp_size,
        strtab_off,
        strtab_size,
        symtab_off,
        symtab_size,
    )
    .unwrap();

    // finally go back and write the header
    const SECTIONS: u16 = 4;
    const STRTAB_INDEX: u16 = 2;
    cursor.set_position(0);
    write_elf_header(&mut cursor, shdrs_off, SECTIONS, STRTAB_INDEX).unwrap();

    buffer
}

fn align_cursor(cursor: &mut Cursor<&mut Vec<u8>>, alignment: usize) -> u64 {
    let pos = cursor.position() as usize;
    let padding = (alignment - (pos % alignment)) % alignment;
    cursor.set_position((pos + padding) as u64);
    cursor.position()
}

fn write_elf_header(
    w: &mut impl Write,
    sh_offset: u64,
    sh_num: u16,
    sh_strndx: u16,
) -> std::io::Result<()> {
    let mut header = [
        0x7f, 0x45, 0x4c, 0x46, // EI_MAG: 0x7F 'ELF'
        0x02, 0x01, 0x01, 0x00, // CLASS64, LSB, Version1
        0x00, 0x00, 0x00, 0x00, // EI_PAD
        0x00, 0x00, 0x00, 0x00, // EI_PAD
        0x01, 0x00, // e_type: ET_REL
        0xf7, 0x00, // e_machine: EM_BPF
        0x01, 0x00, 0x00, 0x00, // e_version: EV_CURRENT
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // e_entry
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // e_phoff
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // e_shoff
        0x00, 0x00, 0x00, 0x00, // e_flags
        0x40, 0x00, // e_ehsize: 64
        0x00, 0x00, // e_phentsize
        0x00, 0x00, // e_phnum
        0x40, 0x00, // e_shentsize: 64
        0x00, 0x00, // e_shnum
        0x00, 0x00, // e_shstrndx
    ];

    header[40..48].copy_from_slice(&sh_offset.to_le_bytes());
    header[60..62].copy_from_slice(&sh_num.to_le_bytes());
    header[62..64].copy_from_slice(&sh_strndx.to_le_bytes());

    w.write_all(&header)
}

#[allow(clippy::too_many_arguments)]
fn write_section_header(
    w: &mut impl Write,
    name: u32,
    type_: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    addralign: u64,
    entsize: u64,
) -> std::io::Result<()> {
    write_fields!(w, name, type_, flags, addr, offset, size, link, info, addralign, entsize);

    Ok(())
}

fn write_symbol(
    w: &mut impl Write,
    name: u32,
    value: u64,
    size: u64,
    info: u8,
    other: u8,
    shndx: u16,
) -> std::io::Result<()> {
    write_fields!(
        w,
        name,
        ((other as u16) << 8) | info as u16,
        shndx,
        value,
        size
    );

    Ok(())
}

// don't format the write_section_headers calls 1-2 digit arguments are annoying
#[rustfmt::skip]
fn write_section_headers(
    w: &mut impl Write,
    xdp_off: u64,
    xdp_size: u64,
    strtab_off: u64,
    strtab_size: u64,
    symtab_off: u64,
    symtab_size: u64,
) -> std::io::Result<()> {
    const STRTAB_XDP_OFF: u32 = 1;
    const STRTAB_SYMTAB_OFF: u32 = 5;
    const STRTAB_STRTAB_OFF: u32 = 13;
    write_section_header(w, 0, SHT_NULL, 0, 0, 0, 0, 0, 0, 0, 0)?;
    write_section_header(w, STRTAB_XDP_OFF, SHT_PROGBITS, SHF_ALLOC | SHF_EXECINSTR, 0, xdp_off, xdp_size, 0, 0, 0, 0)?;
    write_section_header(w, STRTAB_STRTAB_OFF, SHT_STRTAB, 0, 0, strtab_off, strtab_size, 0, 0, 0, 0)?;
    write_section_header(w, STRTAB_SYMTAB_OFF, SHT_SYMTAB, 0, 0, symtab_off, symtab_size, 2, 1, 0, 0)?;
    Ok(())
}
