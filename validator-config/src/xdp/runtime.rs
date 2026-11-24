//! Resolving an XDP policy against the host CPUs.

use {
    super::{XdpEndpoints, XdpWorkerBinding, XdpWorkerPool, collect_worker_queue_ids},
    crate::{Diagnostic, DiagnosticKind, NetworkDeviceSelector, ValidatedConfig},
    std::collections::{BTreeMap, BTreeSet},
};

/// An XDP policy resolved against the host CPUs, for transmitter setup.
#[derive(Clone, Debug)]
pub struct XdpRuntimeConfig {
    interface_label: String,
    device: NetworkDeviceSelector,
    workers: Vec<XdpWorkerBinding>,
    zero_copy: bool,
    sender_positions: XdpEndpoints<Box<[usize]>>,
}

impl XdpRuntimeConfig {
    /// The logical label of the interface used by all endpoints.
    pub fn interface_label(&self) -> &str {
        &self.interface_label
    }

    /// How to select the network device for this interface.
    pub fn device(&self) -> &NetworkDeviceSelector {
        &self.device
    }

    /// Active queue-to-CPU bindings, in declared worker order.
    pub fn workers(&self) -> &[XdpWorkerBinding] {
        &self.workers
    }

    /// Whether AF_XDP zero-copy is requested.
    pub fn zero_copy(&self) -> bool {
        self.zero_copy
    }

    /// Indices into [`Self::workers()`], in each endpoint's sender order.
    /// Each index selects an active worker binding; the binding's
    /// [`XdpWorkerBinding::queue`] field contains its hardware queue ID.
    pub fn sender_positions(&self) -> XdpEndpoints<&[usize]> {
        self.sender_positions.map(Box::as_ref)
    }
}

fn resolve_declared_workers(
    workers: &XdpWorkerPool,
    initial_affinity: &BTreeSet<usize>,
) -> Result<Vec<XdpWorkerBinding>, String> {
    let bind_cpus = |cpus: &[usize]| {
        collect_worker_queue_ids(workers)
            .into_iter()
            .zip(cpus.iter().copied())
            .map(|(queue, cpu)| XdpWorkerBinding { queue, cpu })
            .collect()
    };
    match workers {
        XdpWorkerPool::Auto { count } => {
            let mut eligible: Vec<_> = initial_affinity
                .iter()
                .rev()
                .take(*count)
                .copied()
                .collect();
            if eligible.len() < *count {
                return Err(format!(
                    "not enough CPUs for XDP: workers.auto.count = {count} needs {count} CPUs, \
                     but only {} are available",
                    eligible.len()
                ));
            }
            // Assign queues in ascending CPU order, like an explicit workers.cpus list.
            eligible.reverse();
            Ok(bind_cpus(&eligible))
        }
        XdpWorkerPool::Cpus(cpus) => Ok(bind_cpus(cpus)),
        XdpWorkerPool::Bindings(bindings) => Ok(bindings.clone()),
    }
}

impl ValidatedConfig {
    /// Resolve XDP settings using the validator's initial CPU affinity.
    ///
    /// Automatic workers select CPUs from `initial_affinity`. Active workers
    /// must leave at least one CPU in that set for the main thread. Explicitly
    /// configured worker CPUs can lie outside the initial affinity.
    ///
    /// On Linux, active worker CPU IDs must fit in `cpu_set_t`. Pinning workers
    /// at startup checks whether those CPUs are online and available in the
    /// validator's cpuset.
    ///
    /// Returns `None` when XDP is disabled.
    pub fn resolve_xdp(
        &self,
        initial_affinity: &BTreeSet<usize>,
    ) -> Result<Option<XdpRuntimeConfig>, Diagnostic> {
        let Some(queue_ids) = &self.xdp_queue_ids else {
            return Ok(None);
        };
        if initial_affinity.is_empty() {
            return Err(Diagnostic::new(
                DiagnosticKind::CpuResolution,
                "cannot resolve XDP workers: the process CPU affinity is empty",
            ));
        }
        let (interface_label, interface) = self
            .interfaces()
            .first_key_value()
            .expect("validated enabled XDP config has exactly one interface");
        let workers = &interface.xdp.workers;
        let worker_error = |message| {
            Diagnostic::new(DiagnosticKind::CpuResolution, message)
                .at_field(workers.field_path().iter().copied())
                .at_field(["interfaces", interface_label, "xdp"])
        };
        let declared = resolve_declared_workers(workers, initial_affinity).map_err(worker_error)?;
        let active_ids: BTreeSet<_> = queue_ids
            .get_values()
            .into_iter()
            .flat_map(|queues| queues.iter())
            .copied()
            .collect();
        let active_workers: Vec<_> = declared
            .into_iter()
            .filter(|binding| active_ids.contains(&binding.queue))
            .collect();
        #[cfg(target_os = "linux")]
        if let Some(binding) = active_workers
            .iter()
            .find(|binding| agave_cpu_utils::CpuId::new(binding.cpu).is_err())
        {
            return Err(worker_error(format!(
                "XDP worker CPU {} cannot be represented in a Linux CPU affinity mask",
                binding.cpu
            )));
        }
        if !initial_affinity
            .iter()
            .any(|cpu| !active_workers.iter().any(|binding| binding.cpu == *cpu))
        {
            return Err(Diagnostic::new(
                DiagnosticKind::CpuResolution,
                "XDP workers must leave at least one process CPU unreserved for the main thread",
            ));
        }
        let positions: BTreeMap<_, _> = active_workers
            .iter()
            .enumerate()
            .map(|(position, binding)| (binding.queue, position))
            .collect();
        let sender_positions = queue_ids.map(|queues| {
            queues
                .iter()
                .map(|queue| {
                    *positions
                        .get(queue)
                        .expect("validated selected queue must have an active worker")
                })
                .collect()
        });
        Ok(Some(XdpRuntimeConfig {
            interface_label: interface_label.clone(),
            device: interface.device.clone(),
            workers: active_workers,
            zero_copy: interface.xdp.zero_copy,
            sender_positions,
        }))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::test_utils::{load_config, load_test_config, load_worker_config},
    };

    const ALL_COMPONENTS_QUEUE_ZERO: &str = r#"
schema_version = 1
tpu.xdp.tx.queues = [0]
turbine.xdp.tx.queues = [0]
repair.xdp.tx.queues = [0]
gossip.xdp.tx.queues = [0]
votor.xdp.tx.queues = [0]
"#;

    #[test]
    fn test_sparse_bindings_preserve_worker_and_component_order() {
        // Each component specifies its queue selection and expected sender positions.
        for (case, components, expected_workers) in [
            (
                "all workers",
                XdpEndpoints {
                    gossip: ("[11, 1]", vec![2, 3]),
                    repair: ("[3]", vec![1]),
                    tpu: ("[1, 7]", vec![3, 0]),
                    turbine: ("[7]", vec![0]),
                    votor: ("'all'", vec![0, 1, 2, 3]),
                },
                vec![(7, 8), (3, 9), (11, 10), (1, 11)],
            ),
            (
                "unused middle workers",
                XdpEndpoints {
                    gossip: ("[7, 1]", vec![0, 1]),
                    repair: ("[1]", vec![1]),
                    tpu: ("[1, 7]", vec![1, 0]),
                    turbine: ("[7]", vec![0]),
                    votor: ("[1]", vec![1]),
                },
                vec![(7, 8), (1, 11)],
            ),
        ] {
            let XdpEndpoints {
                gossip: (gossip, expected_gossip),
                repair: (repair, expected_repair),
                tpu: (tpu, expected_tpu),
                turbine: (turbine, expected_turbine),
                votor: (votor, expected_votor),
            } = components;
            let contents = format!(
                r#"
schema_version = 1
gossip.xdp.tx.queues = {gossip}
repair.xdp.tx.queues = {repair}
tpu.xdp.tx.queues = {tpu}
turbine.xdp.tx.queues = {turbine}
votor.xdp.tx.queues = {votor}
[interfaces.primary]
device.name = "eth0"
[interfaces.primary.xdp]
zero_copy = false
workers.bindings = [
    {{ queue = 7, cpu = 8 }},
    {{ queue = 3, cpu = 9 }},
    {{ queue = 11, cpu = 10 }},
    {{ queue = 1, cpu = 11 }},
]
"#
            );
            let runtime = load_test_config(&contents)
                .resolve_xdp(&BTreeSet::from([8, 9, 10, 11, 12]))
                .expect("valid sparse bindings should resolve while preserving sender order")
                .unwrap();
            let expected_workers: Vec<_> = expected_workers
                .into_iter()
                .map(|(queue, cpu)| XdpWorkerBinding { queue, cpu })
                .collect();
            assert_eq!(runtime.workers(), expected_workers, "{case}");
            let senders = runtime.sender_positions();
            assert_eq!(senders.gossip, expected_gossip, "{case}/gossip");
            assert_eq!(senders.repair, expected_repair, "{case}/repair");
            assert_eq!(senders.tpu, expected_tpu, "{case}/tpu");
            assert_eq!(senders.turbine, expected_turbine, "{case}/turbine");
            assert_eq!(senders.votor, expected_votor, "{case}/votor");
        }
    }

    #[test]
    fn test_auto_workers_require_enough_cpus() {
        let config = load_worker_config("{ auto = { count = 2 } }")
            .expect("two-worker fixture should pass host-independent validation");
        let error = config
            .resolve_xdp(&BTreeSet::from([8]))
            .expect_err("auto workers must fail if too few CPUs are available");
        assert_eq!(
            error.to_string(),
            "not enough CPUs for XDP: workers.auto.count = 2 needs 2 CPUs, but only 1 are \
             available",
            "the error should report the requested worker count and available CPUs"
        );
    }

    #[test]
    fn test_auto_workers_use_the_highest_cpus_in_ascending_order() {
        let config = load_worker_config("{ auto = { count = 2 } }")
            .expect("two-worker fixture should pass host-independent validation");
        let runtime = config
            .resolve_xdp(&BTreeSet::from([7, 8, 9, 10, 11]))
            .expect("auto workers should resolve on the highest CPUs")
            .unwrap();
        assert_eq!(
            runtime.workers(),
            [
                XdpWorkerBinding { queue: 0, cpu: 10 },
                XdpWorkerBinding { queue: 1, cpu: 11 },
            ],
            "auto workers should map queues to the selected CPUs in ascending order"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_active_workers_must_fit_the_linux_cpu_affinity_mask() {
        for workers in [
            "{ cpus = [1000000] }",
            "{ bindings = [{ queue = 0, cpu = 1000000 }] }",
        ] {
            let config = load_worker_config(workers).unwrap();
            let error = config
                .resolve_xdp(&BTreeSet::from([0, 1]))
                .expect_err("active worker CPU must fit the Linux affinity mask");
            assert_eq!(error.kind(), DiagnosticKind::CpuResolution);
            assert!(
                error
                    .message()
                    .contains("CPU 1000000 cannot be represented"),
                "{error}"
            );
        }
    }

    #[test]
    fn test_workers_outside_the_affinity_leave_it_unreserved() {
        for (case, workers) in [
            ("cpus", "{ cpus = [9, 11] }"),
            (
                "bindings",
                "{ bindings = [{ queue = 0, cpu = 9 }, { queue = 1, cpu = 11 }] }",
            ),
        ] {
            let config = load_worker_config(workers)
                .expect("worker fixture should pass host-independent validation");
            // Equal worker and affinity CPU counts catch reservation checks based only on counts.
            let runtime = config
                .resolve_xdp(&BTreeSet::from([8, 10]))
                .expect("workers outside the process CPU affinity leave its CPUs unreserved")
                .unwrap();
            assert_eq!(
                runtime.workers(),
                [
                    XdpWorkerBinding { queue: 0, cpu: 9 },
                    XdpWorkerBinding { queue: 1, cpu: 11 },
                ],
                "{case}: explicit worker CPUs outside the affinity should be preserved"
            );
        }
    }

    #[test]
    fn test_every_worker_mode_must_leave_a_cpu_unreserved() {
        for workers in [
            "{ auto = { count = 2 } }",
            "{ cpus = [8, 9] }",
            "{ bindings = [{ queue = 0, cpu = 8 }, { queue = 1, cpu = 9 }] }",
        ] {
            let config = load_worker_config(workers)
                .expect("worker fixture should pass validation before checking CPU reservations");
            let error = config
                .resolve_xdp(&BTreeSet::from([8, 9]))
                .expect_err("active workers must leave at least one process CPU unreserved");
            assert!(
                error.to_string().contains("leave at least one"),
                "{workers}: {error}"
            );
        }
    }

    #[test]
    fn test_empty_cpu_affinity_is_rejected() {
        let config = load_worker_config("{ cpus = [8] }")
            .expect("worker fixture should pass host-independent validation");
        let error = config
            .resolve_xdp(&BTreeSet::new())
            .expect_err("an empty process CPU affinity cannot leave a CPU unreserved");
        assert_eq!(
            error.to_string(),
            "cannot resolve XDP workers: the process CPU affinity is empty"
        );
    }

    #[test]
    fn test_unreferenced_workers_warn_without_reserving_cpus() {
        let (config, warnings) = load_config(&format!(
            r#"
{ALL_COMPONENTS_QUEUE_ZERO}
[interfaces.primary]
device.route = "default"
xdp.zero_copy = false
xdp.workers.cpus = [8, 9]
"#
        ))
        .expect("unreferenced workers should only warn");
        let runtime = config
            .resolve_xdp(&BTreeSet::from([8, 9]))
            .expect("discarding the unreferenced worker should leave a CPU unreserved")
            .unwrap();
        assert_eq!(
            runtime.workers(),
            [XdpWorkerBinding { queue: 0, cpu: 8 }],
            "only the referenced worker should remain in the runtime configuration"
        );
        assert_eq!(
            warnings.len(),
            1,
            "only the unreferenced queue should produce a warning: {warnings:?}"
        );
        assert!(
            ["queue 1 ", "primary", "unreferenced"]
                .iter()
                .all(|fragment| warnings[0].contains(fragment)),
            "warning should identify the unused queue and interface: {warnings:?}"
        );
    }
}
