//! Rust port of Firedancer's fd_hash, used to summarize binary blobs (e.g.
//! ELF sections, serialized VM memory) for conformance comparisons.
//! https://github.com/firedancer-io/firedancer/blob/main/src/util/fd_hash.c

pub fn fd_hash_without_seed(buf: &[u8]) -> u64 {
    fd_hash(0, buf)
}

pub fn fd_hash_u64_without_seed(buf: &[u64]) -> u64 {
    let bytes = unsafe {
        std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), std::mem::size_of_val(buf))
    };
    fd_hash(0, bytes)
}

pub fn fd_hash(seed: u64, buf: &[u8]) -> u64 {
    const C1: u64 = 11400714785074694791;
    const C2: u64 = 14029467366897019727;
    const C3: u64 = 1609587929392839161;
    const C4: u64 = 9650029242287828579;
    const C5: u64 = 2870177450012600261;

    let mut p = buf;
    let sz = buf.len() as u64;
    let mut h: u64;

    if sz < 32 {
        h = seed.wrapping_add(C5);
    } else {
        let mut w = seed.wrapping_add(C1.wrapping_add(C2));
        let mut x = seed.wrapping_add(C2);
        let mut y = seed;
        let mut z = seed.wrapping_sub(C1);

        while p.len() >= 32 {
            let p0 = u64::from_le_bytes(p[0..8].try_into().unwrap());
            let p1 = u64::from_le_bytes(p[8..16].try_into().unwrap());
            let p2 = u64::from_le_bytes(p[16..24].try_into().unwrap());
            let p3 = u64::from_le_bytes(p[24..32].try_into().unwrap());
            w = w
                .wrapping_add(p0.wrapping_mul(C2))
                .rotate_left(31)
                .wrapping_mul(C1);
            x = x
                .wrapping_add(p1.wrapping_mul(C2))
                .rotate_left(31)
                .wrapping_mul(C1);
            y = y
                .wrapping_add(p2.wrapping_mul(C2))
                .rotate_left(31)
                .wrapping_mul(C1);
            z = z
                .wrapping_add(p3.wrapping_mul(C2))
                .rotate_left(31)
                .wrapping_mul(C1);
            p = &p[32..];
        }

        h = w
            .rotate_left(1)
            .wrapping_add(x.rotate_left(7))
            .wrapping_add(y.rotate_left(12))
            .wrapping_add(z.rotate_left(18));

        for v in [w, x, y, z] {
            let vv = v.wrapping_mul(C2).rotate_left(31).wrapping_mul(C1);
            h ^= vv;
            h = h.wrapping_mul(C1).wrapping_add(C4);
        }
    }

    h = h.wrapping_add(sz);

    while p.len() >= 8 {
        let w = u64::from_le_bytes(p[0..8].try_into().unwrap());
        let ww = w.wrapping_mul(C2).rotate_left(31).wrapping_mul(C1);
        h ^= ww;
        h = h.rotate_left(27).wrapping_mul(C1).wrapping_add(C4);
        p = &p[8..];
    }

    if p.len() >= 4 {
        let w = u32::from_le_bytes(p[0..4].try_into().unwrap()) as u64;
        h ^= w.wrapping_mul(C1);
        h = h.rotate_left(23).wrapping_mul(C2).wrapping_add(C3);
        p = &p[4..];
    }

    for &byte in p {
        h ^= (byte as u64).wrapping_mul(C5);
        h = h.rotate_left(11).wrapping_mul(C1);
    }

    h ^= h >> 33;
    h = h.wrapping_mul(C2);
    h ^= h >> 29;
    h = h.wrapping_mul(C3);
    h ^= h >> 32;

    h
}
