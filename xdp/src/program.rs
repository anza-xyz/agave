#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        device::NetworkDevice,
        dispatcher::{EbpfPrograms, XdpDispatcher},
    },
    aya::{Ebpf, EbpfLoader},
    uuid::Uuid,
};

pub enum XdpProgramGuard {
    Dispatcher(XdpDispatcher),
    Ebpf(Ebpf),
}

pub fn load_xdp_program(
    dev: &NetworkDevice,
) -> Result<XdpProgramGuard, Box<dyn std::error::Error>> {
    let mut loader = EbpfLoader::new();

    let broken_frags = if dev.driver()? == "i40e" { 1 } else { 0 };
    loader.set_global("AGAVE_XDP_DROP_MULTI_FRAGS", &broken_frags, true);

    let mut program = EbpfPrograms::new(
        Uuid::new_v4(),
        loader,
        agave_xdp_ebpf::AGAVE_XDP_EBPF_PROGRAM,
    )
    .set_priority("agave_xdp", 0);

    let dispatcher_res = XdpDispatcher::new_with_programs(
        dev.if_index(),
        aya::programs::xdp::XdpFlags::DRV_MODE,
        vec![&mut program],
    );

    match dispatcher_res {
        Ok(dispatcher) => Ok(XdpProgramGuard::Dispatcher(dispatcher)),
        Err(e) => {
            log::warn!(
                "failed to load XDP program via dispatcher {e}, falling back to normal load"
            );
            let ebpf = program
                .loader
                .load(agave_xdp_ebpf::AGAVE_XDP_EBPF_PROGRAM)?;
            Ok(XdpProgramGuard::Ebpf(ebpf))
        }
    }
}
