#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        device::NetworkDevice,
        dispatcher::{EbpfPrograms, XdpDispatcher},
    },
    aya::EbpfLoader,
    uuid::Uuid,
};

pub fn load_xdp_program(dev: &NetworkDevice) -> Result<XdpDispatcher, Box<dyn std::error::Error>> {
    let mut loader = EbpfLoader::new();

    let broken_frags = dev.driver()? == "i40e";
    if broken_frags {
        loader.set_global("AGAVE_XDP_DROP_MULTI_FRAGS", &1u8, true);
    }

    let mut program = EbpfPrograms::new(
        Uuid::new_v4(),
        loader,
        agave_xdp_ebpf::AGAVE_XDP_EBPF_PROGRAM,
    )
    .set_priority("agave_xdp", 0);

    let dispatcher = XdpDispatcher::new_with_programs(
        dev.if_index(),
        aya::programs::xdp::XdpFlags::DRV_MODE,
        vec![&mut program],
    )?;

    Ok(dispatcher)
}
