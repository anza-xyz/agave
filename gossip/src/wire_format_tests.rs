#![allow(clippy::arithmetic_side_effects)]

#[cfg(test)]
mod tests {

    use {
        crate::protocol::Protocol, serde::Serialize,
        solana_net_utils::tooling_for_tests::validate_packet_format, solana_sanitize::Sanitize,
        std::path::PathBuf,
    };

    fn parse_gossip(bytes: &[u8]) -> anyhow::Result<Protocol> {
        let pkt: Protocol = solana_perf::packet::deserialize_from_with_limit(bytes)?;
        pkt.sanitize()?;
        Ok(pkt)
    }

    fn serialize<T: Serialize>(pkt: T) -> Vec<u8> {
        bincode::serialize(&pkt).unwrap()
    }

    /// Test the ability of gossip parsers to understand and re-serialize a corpus of
    /// packets captured from mainnet.
    /// You will want to run this in --release mode
    /// This test requires external files and is not run by default.
    /// `cargo test test_gossip_wire_format  -- --nocapture --ignored` to run this test manually
    #[test]
    #[ignore]
    fn test_gossip_wire_format() {
        solana_logger::setup();
        let path_base = match std::env::var_os("CARGO_MANIFEST_DIR") {
            Some(p) => PathBuf::from(p),
            None => panic!("Requires CARGO_MANIFEST_DIR env variable"),
        };
        let path_base = path_base.join("GOSSIP_PACKETS");
        for entry in std::fs::read_dir(path_base).unwrap() {
            let entry = entry.unwrap();
            validate_packet_format(&entry.path(), parse_gossip, serialize).unwrap();
        }
    }
}
