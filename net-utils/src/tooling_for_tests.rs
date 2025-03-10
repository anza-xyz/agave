#![allow(clippy::arithmetic_side_effects)]
use {
    anyhow::Context,
    log::{debug, error, info},
    pcap_file::pcapng::PcapNgReader,
    std::{fs::File, io::Write, path::PathBuf},
};

/// Prints a hexdump of a given byte buffer into stdout
pub fn hexdump(bytes: &[u8]) -> anyhow::Result<()> {
    hxdmp::hexdump(bytes, &mut std::io::stderr())?;
    std::io::stderr().write_all(b"\n")?;
    Ok(())
}

/// Reads all packets from PCAPNG file
pub struct PcapReader {
    reader: PcapNgReader<File>,
}
impl PcapReader {
    pub fn new(filename: &PathBuf) -> anyhow::Result<Self> {
        let file_in = File::open(filename).with_context(|| format!("opening file {filename:?}"))?;
        let reader = PcapNgReader::new(file_in).context("pcap reader creation")?;

        Ok(PcapReader { reader })
    }
}

impl Iterator for PcapReader {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let block = match self.reader.next_block() {
                Some(block) => block.ok()?,
                None => return None,
            };
            let data = match block {
                pcap_file::pcapng::Block::Packet(ref block) => {
                    &block.data[0..block.original_len as usize]
                }
                pcap_file::pcapng::Block::SimplePacket(ref block) => {
                    &block.data[0..block.original_len as usize]
                }
                pcap_file::pcapng::Block::EnhancedPacket(ref block) => {
                    &block.data[0..block.original_len as usize]
                }
                _ => {
                    debug!("Skipping unknown block in pcap file");
                    continue;
                }
            };

            let pkt_payload = data;
            // Check if IP header is present, if it is we can safely skip it
            // let pkt_payload = if data[0] == 69 {
            //     &data[20 + 8..]
            // } else {
            //     &data[0..]
            // };
            //return Some(data.to_vec().into_boxed_slice());
            return Some(pkt_payload.to_vec());
        }
    }
}

pub fn validate_packet_format<T, P, S>(
    filename: &PathBuf,
    parse_packet: P,
    serialize_packet: S,
) -> anyhow::Result<usize>
where
    P: Fn(&[u8]) -> anyhow::Result<T>,
    S: Fn(T) -> Vec<u8>,
{
    info!(
        "Validating packet format for {} using samples from {filename:?}",
        std::any::type_name::<T>()
    );
    let reader = PcapReader::new(filename)?;
    let mut number = 0;
    let mut errors = 0;
    for data in reader.into_iter() {
        number += 1;
        match parse_packet(&data) {
            Ok(pkt) => {
                let reconstructed_bytes = serialize_packet(pkt);
                if reconstructed_bytes != data {
                    errors += 1;
                    error!("Reserialization failed for packet {number} in {filename:?}!");
                    error!("Original packet bytes:");
                    hexdump(&data)?;
                    error!("Reserialized bytes:");
                    hexdump(&reconstructed_bytes)?;
                    break;
                }
            }
            Err(e) => {
                errors += 1;
                error!("Found packet {number} that failed to parse with error {e}");
                error!("Problematic packet bytes:");
                hexdump(&data)?;
                break;
            }
        }
    }
    info!("Packet format checks passed for {number} packets, failed for {errors} packets.");
    if errors > 0 {
        Err(anyhow::anyhow!("Failed checks for {errors} packets"))
    } else {
        Ok(number)
    }
}
