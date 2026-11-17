use {
    caps::{
        CapSet,
        Capability::{self, CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW, CAP_PERFMON, CAP_SYS_NICE},
        CapsHashSet,
    },
    log::warn,
    std::error::Error,
};

const SUPPORTED_CAPS: [Capability; 5] = [
    CAP_BPF,
    CAP_NET_ADMIN,
    CAP_NET_RAW,
    CAP_PERFMON,
    CAP_SYS_NICE,
];

#[derive(Debug, PartialEq, Eq)]
pub(super) enum XdpMode {
    Disabled,
    Copy,
    ZeroCopy,
}

/// Drops every capability the configuration does not require, holding the ones XDP needs only
/// while `with_xdp_caps` runs. Must be called before the validator spawns any threads, so that no
/// thread keeps the XDP capabilities and leaves the process usable as a privilege escalation gadget.
pub(super) fn restrict<T>(
    primordial_caps: CapsHashSet,
    needs_sys_nice: bool,
    xdp_mode: XdpMode,
    with_xdp_caps: impl FnOnce() -> T,
) -> Result<T, Box<dyn Error>> {
    let (required_caps, retained_caps) =
        compute_capability_sets(&primordial_caps, needs_sys_nice, xdp_mode);
    let mut supported_caps = primordial_caps;
    supported_caps.extend(SUPPORTED_CAPS);

    let permitted_caps = caps::read(None, CapSet::Permitted)
        .map_err(|err| format!("failed to read permitted capabilities: {err}"))?;
    let missing_caps: Vec<_> = required_caps.difference(&permitted_caps).collect();
    if !missing_caps.is_empty() {
        return Err(format!(
            "the current configuration requires the following capabilities, which have not been \
             permitted to the current process: {missing_caps:?}"
        )
        .into());
    }
    let extra_caps: Vec<_> = permitted_caps.difference(&supported_caps).collect();
    if !extra_caps.is_empty() {
        warn!(
            "dropping extraneous capabilities ({extra_caps:?}) from the current process. consider \
             removing them from your operational configuration.",
        );
    }

    set_caps(&required_caps)?;
    let result = with_xdp_caps();
    // we're done with caps needed to init xdp now. remove them from our process
    set_caps(&retained_caps)?;
    Ok(result)
}

/// Returns the capabilities required while setting up XDP and the ones retained afterwards
fn compute_capability_sets(
    primordial_caps: &CapsHashSet,
    needs_sys_nice: bool,
    xdp_mode: XdpMode,
) -> (CapsHashSet, CapsHashSet) {
    let mut retained_caps = primordial_caps.clone();
    if needs_sys_nice {
        retained_caps.insert(CAP_SYS_NICE);
    }
    let mut required_caps = retained_caps.clone();
    match xdp_mode {
        XdpMode::Disabled => (),
        XdpMode::Copy => required_caps.extend([CAP_NET_ADMIN, CAP_NET_RAW]),
        XdpMode::ZeroCopy => {
            required_caps.extend([CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON]);
        }
    }
    (required_caps, retained_caps)
}

fn set_caps(caps: &CapsHashSet) -> Result<(), Box<dyn Error>> {
    for set in [CapSet::Effective, CapSet::Permitted] {
        caps::set(None, set, caps)
            .map_err(|err| format!("failed to set {set:?} capabilities: {err}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {super::*, caps::Capability::CAP_DAC_OVERRIDE, test_case::test_case};

    #[test_case(XdpMode::Disabled, false, &[], &[]; "disabled")]
    #[test_case(XdpMode::Disabled, true, &[CAP_SYS_NICE], &[CAP_SYS_NICE]; "disabled_with_niceness")]
    #[test_case(XdpMode::Copy, false, &[CAP_NET_ADMIN, CAP_NET_RAW], &[]; "copy")]
    #[test_case(XdpMode::Copy, true, &[CAP_NET_ADMIN, CAP_NET_RAW, CAP_SYS_NICE], &[CAP_SYS_NICE]; "copy_with_niceness")]
    #[test_case(XdpMode::ZeroCopy, false, &[CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON], &[]; "zero_copy")]
    #[test_case(XdpMode::ZeroCopy, true, &[CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON, CAP_SYS_NICE], &[CAP_SYS_NICE]; "zero_copy_with_niceness")]
    fn test_compute_capability_sets(
        xdp_mode: XdpMode,
        needs_sys_nice: bool,
        required_caps: &[Capability],
        retained_caps: &[Capability],
    ) {
        let with_primordial = |caps: &[Capability]| -> CapsHashSet {
            caps.iter().copied().chain([CAP_DAC_OVERRIDE]).collect()
        };
        assert_eq!(
            compute_capability_sets(&with_primordial(&[]), needs_sys_nice, xdp_mode),
            (
                with_primordial(required_caps),
                with_primordial(retained_caps)
            )
        );
    }

    #[test]
    fn test_restrict_without_xdp() {
        // Capabilities are per thread, so the dropped ones only affect this thread
        std::thread::spawn(|| {
            assert_eq!(
                restrict(CapsHashSet::new(), false, XdpMode::Disabled, || 42).unwrap(),
                42
            );
            for set in [CapSet::Effective, CapSet::Permitted] {
                assert!(caps::read(None, set).unwrap().is_empty());
            }
        })
        .join()
        .unwrap();
    }
}
