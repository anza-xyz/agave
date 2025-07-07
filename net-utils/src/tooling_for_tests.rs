#![allow(clippy::arithmetic_side_effects)]
use {
    anyhow::Context,
    log::{debug, error, info},
    pcap_file::pcapng::PcapNgReader,
    std::{fs::File, io::Write, path::PathBuf},
};

/// Prints a hexdump of a given byte buffer into stderr
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
            return Some(pkt_payload.to_vec());
        }
    }
}

/// Helper function to validate packet parsing capabilities across agave.
/// It will read all packets from file identified by `filename`, then parse them
/// using parse_packet, re-serialize using `serialize_packet`, and finally compare
/// whether the original packet matches the reserialized version.
/// If parser returns errors, the test fails and offending packet is reported.
/// If any differences are found using `custom_compare`, they are reported as errors.
///
/// Note that no matter how many packets are present in a given file, one can never be 100%
/// certain this will catch all wire format issues.
pub fn validate_packet_format<T>(
    filename: &PathBuf,
    parse_packet: fn(&[u8]) -> anyhow::Result<T>,
    serialize_packet: fn(T) -> Vec<u8>,
    show_packet: fn(&[u8]) -> anyhow::Result<()>,
    custom_compare: fn(&[u8], &[u8]) -> Option<usize>,
) -> anyhow::Result<usize>
where
    T: Sized,
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
        let packet = parse_packet(&data);
        match packet {
            Ok(pkt) => {
                let reconstructed_bytes = serialize_packet(pkt);
                let diff = custom_compare(&reconstructed_bytes, &data);
                if let Some(pos) = diff {
                    errors += 1;
                    error!(
                        "Reserialization differences found for packet {number} in {filename:?}!"
                    );
                    error!("Differences start at byte {pos}");
                    error!("Original packet:");
                    show_packet(&data)?;
                    error!("Reserialized:");
                    show_packet(&reconstructed_bytes)?;
                    break;
                }
            }
            Err(e) => {
                errors += 1;
                error!("Found packet {number} that failed to parse with error {e}");
                error!("Problematic packet:");
                show_packet(&data)?;
                break;
            }
        }
    }
    if errors > 0 {
        error!("Packet format checks passed for {number} packets, failed for {errors} packets.");
        Err(anyhow::anyhow!("Failed checks for {errors} packets"))
    } else {
        info!("Packet format checks passed for {number} packets.");
        Ok(number)
    }
}
