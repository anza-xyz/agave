//! Gossip conformance harness.
//!
//! Decodes a binary-encoded gossip message into a protobuf-encoded
//! `GossipEffects`, mirroring the wire-format decode path so the result can be
//! compared against other validator implementations.

use {
    bv::Bits,
    prost::Message,
    protosol::protos::{
        GossipBloom, GossipCompressedSlots, GossipContactInfo, GossipCrdsData, GossipCrdsFilter,
        GossipCrdsValue, GossipDuplicateShred, GossipEffects, GossipEpochSlots,
        GossipIncrementalHash, GossipIpAddr, GossipIpv6Addr, GossipLowestSlot, GossipMsg,
        GossipPing, GossipPong, GossipPruneData, GossipPruneMessage, GossipPullRequest,
        GossipPullResponse, GossipPushMessage, GossipSnapshotHashes, GossipSocketEntry, GossipVote,
        gossip_compressed_slots, gossip_crds_data, gossip_ip_addr, gossip_msg,
    },
    solana_gossip::{
        Ping, Protocol, PruneData,
        crds_data::CrdsData,
        crds_gossip_pull::CrdsFilter as NativeCrdsFilter,
        crds_value::CrdsValue as NativeCrdsValue,
        deserialize_protocol,
        ping_pong::Pong,
    },
    solana_keypair::signable::Signable,
    solana_packet::PACKET_DATA_SIZE,
    solana_sanitize::Sanitize,
    std::ffi::c_int,
};

pub fn gossip_decode_to_effects(input: &[u8]) -> GossipEffects {
    fn convert_ping(ping: &Ping) -> gossip_msg::Msg {
        gossip_msg::Msg::Ping(GossipPing {
            from: ping.pubkey().to_bytes().to_vec(),
            token: ping.signable_data().to_vec(),
            signature: ping.get_signature().as_ref().to_vec(),
        })
    }

    fn convert_pong(pong: &Pong) -> gossip_msg::Msg {
        gossip_msg::Msg::Pong(GossipPong {
            from: pong.from().to_bytes().to_vec(),
            hash: pong.signable_data().to_vec(),
            signature: pong.get_signature().as_ref().to_vec(),
        })
    }

    fn convert_bloom(bloom: &solana_bloom::bloom::Bloom<solana_hash::Hash>) -> GossipBloom {
        let mut bits_bytes = Vec::new();
        for i in 0..bloom.bits.block_len() {
            bits_bytes.extend_from_slice(&bloom.bits.get_block(i).to_le_bytes());
        }
        GossipBloom {
            keys: bloom.keys.clone(),
            bits: bits_bytes,
            num_bits_set: bloom.num_bits_set(),
        }
    }

    fn convert_crds_filter(filter: &NativeCrdsFilter) -> GossipCrdsFilter {
        let mask_bits = filter.get_mask_bits();
        GossipCrdsFilter {
            filter: Some(convert_bloom(&filter.filter)),
            mask: NativeCrdsFilter::canonical_mask(filter.mask(), mask_bits),
            mask_bits,
        }
    }

    fn convert_crds_data(data: &CrdsData) -> GossipCrdsData {
        let converted = match data {
            CrdsData::ContactInfo(ci) => {
                let v = ci.version();
                Some(gossip_crds_data::Data::ContactInfo(GossipContactInfo {
                    pubkey: ci.pubkey().to_bytes().to_vec(),
                    wallclock: ci.wallclock(),
                    outset: ci.outset(),
                    shred_version: ci.shred_version() as u32,
                    version_major: v.major() as u32,
                    version_minor: v.minor() as u32,
                    version_patch: v.patch() as u32,
                    version_commit: v.commit(),
                    version_feature_set: v.feature_set(),
                    version_client: u16::try_from(v.client().clone()).expect("valid client id")
                        as u32,
                    addrs: ci
                        .addrs()
                        .iter()
                        .map(|a| GossipIpAddr {
                            addr: Some(match a {
                                std::net::IpAddr::V4(v4) => {
                                    gossip_ip_addr::Addr::Ipv4(u32::from(*v4))
                                }
                                std::net::IpAddr::V6(v6) => {
                                    let octets = v6.octets();
                                    gossip_ip_addr::Addr::Ipv6(GossipIpv6Addr {
                                        hi: u64::from_be_bytes(octets[..8].try_into().unwrap()),
                                        lo: u64::from_be_bytes(octets[8..].try_into().unwrap()),
                                    })
                                }
                            }),
                        })
                        .collect(),
                    sockets: ci
                        .sockets()
                        .iter()
                        .map(|s| GossipSocketEntry {
                            key: s.key as u32,
                            index: s.index as u32,
                            offset: s.offset as u32,
                        })
                        .collect(),
                    extensions: vec![],
                }))
            }
            CrdsData::Vote(idx, vote) => {
                let tx_bytes =
                    wincode::serialize(vote.transaction()).expect("vote transaction serialization");
                Some(gossip_crds_data::Data::Vote(GossipVote {
                    index: *idx as u32,
                    from: vote.from().to_bytes().to_vec(),
                    wallclock: vote.wallclock(),
                    transaction: tx_bytes,
                }))
            }
            CrdsData::LowestSlot(_, ls) => {
                Some(gossip_crds_data::Data::LowestSlot(GossipLowestSlot {
                    index: 0,
                    from: ls.from().to_bytes().to_vec(),
                    lowest: ls.lowest,
                    wallclock: ls.wallclock(),
                }))
            }
            CrdsData::EpochSlots(idx, es) => {
                use solana_gossip::epoch_slots::CompressedSlots;
                let slots = es
                    .slots
                    .iter()
                    .map(|cs| match cs {
                        CompressedSlots::Uncompressed(u) => {
                            let mut bits = Vec::new();
                            for i in 0..u.slots.block_len() {
                                bits.extend_from_slice(&u.slots.get_block(i).to_le_bytes());
                            }
                            GossipCompressedSlots {
                                first_slot: u.first_slot,
                                num: u.num as u64,
                                data: Some(gossip_compressed_slots::Data::Uncompressed(bits)),
                            }
                        }
                        CompressedSlots::Flate2(f) => GossipCompressedSlots {
                            first_slot: f.first_slot,
                            num: f.num as u64,
                            data: Some(gossip_compressed_slots::Data::Flate2(
                                f.compressed.to_vec(),
                            )),
                        },
                    })
                    .collect();
                Some(gossip_crds_data::Data::EpochSlots(GossipEpochSlots {
                    index: *idx as u32,
                    from: es.from.to_bytes().to_vec(),
                    wallclock: es.wallclock,
                    slots,
                }))
            }
            CrdsData::SnapshotHashes(sh) => Some(gossip_crds_data::Data::SnapshotHashes(
                GossipSnapshotHashes {
                    from: sh.from.to_bytes().to_vec(),
                    full_slot: sh.full.0,
                    full_hash: sh.full.1.to_bytes().to_vec(),
                    incremental: sh
                        .incremental
                        .iter()
                        .map(|(slot, hash)| GossipIncrementalHash {
                            slot: *slot,
                            hash: hash.to_bytes().to_vec(),
                        })
                        .collect(),
                    wallclock: sh.wallclock,
                },
            )),
            CrdsData::DuplicateShred(idx, ds) => Some(gossip_crds_data::Data::DuplicateShred(
                GossipDuplicateShred {
                    index: *idx as u32,
                    from: ds.from().to_bytes().to_vec(),
                    wallclock: ds.wallclock(),
                    slot: ds.slot(),
                    shred_index: 0,
                    shred_type: 0,
                    num_chunks: ds.num_chunks() as u32,
                    chunk_index: ds.chunk_index() as u32,
                    chunk: ds.chunk().to_vec(),
                },
            )),
            CrdsData::LegacyContactInfo(..)
            | CrdsData::LegacySnapshotHashes(..)
            | CrdsData::AccountsHashes(..)
            | CrdsData::LegacyVersion(..)
            | CrdsData::Version(..)
            | CrdsData::NodeInstance(..)
            | CrdsData::RestartLastVotedForkSlots(..)
            | CrdsData::RestartHeaviestFork(..) => None,
        };
        GossipCrdsData { data: converted }
    }

    fn convert_crds_value(value: &NativeCrdsValue) -> GossipCrdsValue {
        GossipCrdsValue {
            signature: value.get_signature().as_ref().to_vec(),
            data: Some(convert_crds_data(value.data())),
        }
    }

    fn convert_prune_data(pd: &PruneData) -> GossipPruneData {
        GossipPruneData {
            pubkey: pd.pubkey.to_bytes().to_vec(),
            prunes: pd.prunes.iter().map(|p| p.to_bytes().to_vec()).collect(),
            signature: pd.signature.as_ref().to_vec(),
            destination: pd.destination.to_bytes().to_vec(),
            wallclock: pd.wallclock,
        }
    }

    fn convert_protocol(proto: &Protocol) -> gossip_msg::Msg {
        match proto {
            Protocol::PingMessage(ping) => convert_ping(ping),
            Protocol::PongMessage(pong) => convert_pong(pong),
            Protocol::PullRequest(filter, value) => {
                gossip_msg::Msg::PullRequest(GossipPullRequest {
                    filter: Some(convert_crds_filter(filter)),
                    value: Some(convert_crds_value(value)),
                })
            }
            Protocol::PullResponse(pubkey, values) => {
                gossip_msg::Msg::PullResponse(GossipPullResponse {
                    pubkey: pubkey.to_bytes().to_vec(),
                    values: values.iter().map(convert_crds_value).collect(),
                })
            }
            Protocol::PushMessage(pubkey, values) => {
                gossip_msg::Msg::PushMessage(GossipPushMessage {
                    pubkey: pubkey.to_bytes().to_vec(),
                    values: values.iter().map(convert_crds_value).collect(),
                })
            }
            Protocol::PruneMessage(pubkey, data) => {
                gossip_msg::Msg::PruneMessage(GossipPruneMessage {
                    pubkey: pubkey.to_bytes().to_vec(),
                    data: Some(convert_prune_data(data)),
                })
            }
        }
    }

    // Reject over-long inputs upfront so an over-long input whose prefix
    // encodes a valid Protocol can't slip through; deserialize_exact rejects
    // any trailing bytes that remain within the packet window.
    if input.len() > PACKET_DATA_SIZE {
        return GossipEffects {
            valid: false,
            msg: None,
        };
    }
    let result = deserialize_protocol(input);

    match result {
        Ok(proto) if proto.sanitize().is_ok() => {
            let msg = convert_protocol(&proto);
            GossipEffects {
                valid: true,
                msg: Some(GossipMsg { msg: Some(msg) }),
            }
        }
        _ => GossipEffects {
            valid: false,
            msg: None,
        },
    }
}

/// Conformance harness entry point for gossip message decoding.
///
/// Deserializes a binary-encoded gossip message from the input buffer,
/// decodes it into effects, and writes the Protobuf-encoded result into
/// the output buffer.
///
/// # Parameters
/// - `out_ptr`: Pointer to the output buffer where the Protobuf-encoded
///   result will be written.
/// - `out_psz`: Pointer to a `u64` that on entry holds the capacity of the
///   output buffer and on successful return is updated to the number of
///   bytes actually written.
/// - `in_ptr`: Pointer to the input buffer containing the binary-encoded
///   gossip message. May be null when `in_sz` is 0.
/// - `in_sz`: Length of the input buffer in bytes.
///
/// # Returns
/// `1` on success, `0` on failure (null pointers, zero-length input, or
/// output buffer too small).
///
/// # Safety
/// - `out_ptr` must point to a writable buffer whose capacity is stored at
///   `*out_psz`.
/// - `out_psz` must be a valid, aligned pointer to a `u64`.
/// - `in_ptr` must point to a readable buffer of at least `in_sz` bytes
///   (or be null/ignored when `in_sz` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_gossip_decode_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *const u8,
    in_sz: u64,
) -> c_int {
    if out_ptr.is_null() || out_psz.is_null() {
        return 0;
    }

    let in_slice = if in_sz == 0 {
        &[]
    } else if in_ptr.is_null() {
        return 0;
    } else {
        unsafe { std::slice::from_raw_parts(in_ptr, in_sz as usize) }
    };

    let effects = gossip_decode_to_effects(in_slice);
    let out_vec = effects.encode_to_vec();

    let out_cap = unsafe { *out_psz } as usize;
    if out_vec.len() > out_cap {
        return 0;
    }
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, out_cap) };
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    unsafe { *out_psz = out_vec.len() as u64 };
    1
}
