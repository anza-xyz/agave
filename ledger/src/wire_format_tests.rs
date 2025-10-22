#![allow(clippy::arithmetic_side_effects)]

#[cfg(test)]
mod tests {
    use {
        crate::shred::Shred,
        solana_net_utils::tooling_for_tests::{hexdump, validate_packet_format},
        std::path::PathBuf,
    };

    fn parse_turbine(bytes: &[u8]) -> anyhow::Result<Shred> {
        let shred = Shred::new_from_serialized_shred(bytes.to_owned())
            .map_err(|_e| anyhow::anyhow!("Can not deserialize"))?;
        Ok(shred)
    }

    fn serialize(pkt: Shred) -> Vec<u8> {
        pkt.payload().to_vec()
    }

    fn find_differences(a: &[u8], b: &[u8]) -> Option<usize> {
        if a.len() != b.len() {
            return Some(a.len());
        }
        for (idx, (e1, e2)) in a.iter().zip(b).enumerate() {
            if e1 != e2 {
                return Some(idx);
            }
        }
        None
    }

    fn show_packet(bytes: &[u8]) -> anyhow::Result<()> {
        let shred = parse_turbine(bytes)?;
        let merkle_root = shred.merkle_root();
        let chained_merkle_root = shred.chained_merkle_root();
        let rtx_sign = shred.retransmitter_signature();

        println!("=== {} bytes ===", bytes.len());
        println!(
            "Shred ID={ID:?} ErasureSetID={ESI:?}",
            ID = shred.id(),
            ESI = shred.erasure_set()
        );
        println!(
            "Shred merkle root {:X?}, chained root {:X?}, rtx_sign {:X?}",
            merkle_root.map(|v| v.as_ref().to_vec()),
            chained_merkle_root.map(|v| v.as_ref().to_vec()),
            rtx_sign.map(|v| v.as_ref().to_vec())
        );
        println!(
            "Data shreds: {:?}, Coding shreds: {:?}",
            shred.num_data_shreds(),
            shred.num_coding_shreds()
        );
        hexdump(bytes)?;
        println!("===");
        Ok(())
    }

    /// Test the ability of turbine parser to understand and re-serialize a corpus of
    /// packets captured from mainnet.
    ///
    /// This test requires external files and is not run by default.
    /// Export the "TURBINE_WIRE_FORMAT_PACKETS" env variable to run this test.
    #[test]
    fn test_turbine_wire_format() {
        agave_logger::setup();
        let path_base = match std::env::var_os("TURBINE_WIRE_FORMAT_PACKETS") {
            Some(p) => PathBuf::from(p),
            None => {
                eprintln!("Test requires TURBINE_WIRE_FORMAT_PACKETS env variable, skipping!");
                return;
            }
        };
        for entry in
            std::fs::read_dir(path_base).expect("Expecting env var to point to a directory")
        {
            let entry = entry.expect("Expecting a readable file");
            validate_packet_format(
                &entry.path(),
                parse_turbine,
                serialize,
                show_packet,
                find_differences,
            )
            .unwrap();
        }
    }
}
