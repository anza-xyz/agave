#![allow(clippy::arithmetic_side_effects)]

#[cfg(test)]
mod tests {

    use {
        crate::protocol::Protocol,
        serde::Serialize,
        solana_net_utils::tooling_for_tests::{hexdump, validate_packet_format},
        solana_sanitize::Sanitize,
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

    fn find_differences(a: &[u8], b: &[u8]) -> Option<usize> {
        if a.len() != b.len() {
            return Some(a.len().min(b.len()));
        }
        for (idx, (e1, e2)) in a.iter().zip(b).enumerate() {
            if e1 != e2 {
                return Some(idx);
            }
        }
        None
    }

    /// Test the ability of gossip parsers to understand and re-serialize a corpus of
    /// packets captured from mainnet.
    ///
    /// This test requires external files and is not run by default.
    /// Export the "GOSSIP_WIRE_FORMAT_PACKETS" variable to run this test
    #[test]
    fn test_gossip_wire_format() {
        agave_logger::setup();
        let path_base = match std::env::var_os("GOSSIP_WIRE_FORMAT_PACKETS") {
            Some(p) => PathBuf::from(p),
            None => {
                eprintln!("Test requires GOSSIP_WIRE_FORMAT_PACKETS env variable, skipping!");
                return;
            }
        };
        for entry in
            std::fs::read_dir(path_base).expect("Expecting env var to point to a directory")
        {
            let entry = entry.expect("Expecting a readable file");
            validate_packet_format(
                &entry.path(),
                parse_gossip,
                serialize,
                hexdump,
                find_differences,
            )
            .unwrap();
        }
    }
}
