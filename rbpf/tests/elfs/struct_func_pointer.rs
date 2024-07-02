struct PubkeyLutEntry {
    _fp: fn(u8) -> u8,
    key: u64,
}

fn f1(a: u8) -> u8 {
    return a + 1;
}

#[link_section = ".data.rel.ro"]
static E1: PubkeyLutEntry = PubkeyLutEntry { _fp: f1, key: 0x0102030405060708 };

#[no_mangle]
pub fn entrypoint() -> u64 {
    return E1.key;
}
