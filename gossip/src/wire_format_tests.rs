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

    #[test]
    fn validate_ping() {
        solana_logger::setup();
        let path_base = match std::env::var_os("CARGO_MANIFEST_DIR") {
            Some(p) => PathBuf::from(p),
            None => panic!("Requires CARGO_MANIFEST_DIR env variable"),
        };

        validate_packet_format(
            &path_base.join("GOSSIP_PACKETS/ping.pcap"),
            parse_gossip,
            serialize,
        )
        .unwrap();
        validate_packet_format(
            &path_base.join("GOSSIP_PACKETS/pull_request.pcap"),
            parse_gossip,
            serialize,
        )
        .unwrap();
    }
}
