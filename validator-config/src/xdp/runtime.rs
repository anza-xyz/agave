//! Resolving an XDP policy against the host CPUs.

use {
    super::{XdpEndpoints, XdpWorkerBinding, XdpWorkerPool, collect_worker_queue_ids},
    crate::{Diagnostic, DiagnosticKind, NetworkDeviceSelector, ValidatedConfig},
    std::collections::{BTreeMap, BTreeSet},
};

/// An XDP policy resolved against the host CPUs, for transmitter setup.
#[derive(Clone, Debug)]
pub struct XdpRuntimeConfig {
    pub interface_label: String,
    pub device: NetworkDeviceSelector,
    /// Active queue-to-CPU bindings, in declared worker order.
    pub queues: Vec<XdpWorkerBinding>,
    pub zero_copy: bool,
    /// Indices into [`Self::queues`], in each endpoint's sender order.
    /// Each index selects an active worker binding; the binding's
    /// [`XdpWorkerBinding::queue`] field contains its hardware queue ID.
    pub endpoints: XdpEndpoints<Box<[usize]>>,
}

fn resolve_declared_workers(
    workers: &XdpWorkerPool,
    allowed_cpus: &BTreeSet<usize>,
    poh_core: Option<usize>,
) -> Result<Vec<XdpWorkerBinding>, String> {
    let reject_poh_core = |cpus: Vec<usize>| match poh_core {
        Some(poh_core) if cpus.contains(&poh_core) => {
            Err(format!("XDP worker CPU {poh_core} overlaps the PoH core"))
        }
        _ => Ok(cpus),
    };
    // Each mode contributes only the CPU order; collect_worker_queue_ids owns the queue-id
    // rule so validation and resolution cannot disagree about the declared pool.
    let cpus: Vec<usize> = match workers {
        XdpWorkerPool::Auto { count } => {
            let mut eligible: Vec<_> = allowed_cpus
                .iter()
                .rev()
                .copied()
                .filter(|cpu| Some(*cpu) != poh_core)
                .take(*count)
                .collect();
            if eligible.len() < *count {
                return Err(format!(
                    "not enough CPUs for XDP: workers.auto.count = {count} needs {count} CPUs \
                     other than the PoH core, but only {} are available",
                    eligible.len()
                ));
            }
            // Assign queues in ascending CPU order, like an explicit workers.cpus list.
            eligible.reverse();
            eligible
        }
        XdpWorkerPool::Cpus(cpus) => reject_poh_core(cpus.clone())?,
        XdpWorkerPool::Bindings(bindings) => {
            reject_poh_core(bindings.iter().map(|binding| binding.cpu).collect())?
        }
    };
    Ok(collect_worker_queue_ids(workers)
        .into_iter()
        .zip(cpus)
        .map(|(queue, cpu)| XdpWorkerBinding { queue, cpu })
        .collect())
}

impl ValidatedConfig {
    /// Resolve the enabled XDP settings against the host CPUs.
    /// Returns `None` when XDP is disabled.
    /// All declared workers must avoid the PoH core, including unreferenced workers.
    /// On Linux, active workers must fit the transmitter's CPU affinity mask.
    pub fn resolve_xdp(
        &self,
        allowed_cpus: &BTreeSet<usize>,
        poh_core: Option<usize>,
    ) -> Result<Option<XdpRuntimeConfig>, Diagnostic> {
        let Some(queue_ids) = &self.xdp_queue_ids else {
            return Ok(None);
        };
        if allowed_cpus.is_empty() {
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
        let declared =
            resolve_declared_workers(workers, allowed_cpus, poh_core).map_err(worker_error)?;
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
                "XDP worker CPU {} is outside the Linux CPU affinity mask",
                binding.cpu
            )));
        }
        if !allowed_cpus.iter().any(|cpu| {
            Some(*cpu) != poh_core && !active_workers.iter().any(|binding| binding.cpu == *cpu)
        }) {
            return Err(Diagnostic::new(
                DiagnosticKind::CpuResolution,
                "XDP workers and the PoH core must leave at least one process CPU unreserved for \
                 the main thread",
            ));
        }
        let positions: BTreeMap<_, _> = active_workers
            .iter()
            .enumerate()
            .map(|(position, binding)| (binding.queue, position))
            .collect();
        let endpoints = queue_ids.map(|queues| {
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
            queues: active_workers,
            zero_copy: interface.xdp.zero_copy,
            endpoints,
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
                .resolve_xdp(&BTreeSet::from([8, 9, 10, 11, 12]), None)
                .expect("valid sparse bindings should resolve while preserving sender order")
                .unwrap();
            let expected_workers: Vec<_> = expected_workers
                .into_iter()
                .map(|(queue, cpu)| XdpWorkerBinding { queue, cpu })
                .collect();
            assert_eq!(runtime.queues, expected_workers, "{case}");
            assert_eq!(
                runtime.endpoints.gossip.as_ref(),
                expected_gossip,
                "{case}/gossip"
            );
            assert_eq!(
                runtime.endpoints.repair.as_ref(),
                expected_repair,
                "{case}/repair"
            );
            assert_eq!(runtime.endpoints.tpu.as_ref(), expected_tpu, "{case}/tpu");
            assert_eq!(
                runtime.endpoints.turbine.as_ref(),
                expected_turbine,
                "{case}/turbine"
            );
            assert_eq!(
                runtime.endpoints.votor.as_ref(),
                expected_votor,
                "{case}/votor"
            );
        }
    }

    #[test]
    fn test_active_explicit_workers_must_avoid_the_poh_core() {
        for (case, workers) in [
            ("cpus", "{ cpus = [9] }"),
            ("bindings", "{ bindings = [{ queue = 0, cpu = 9 }] }"),
        ] {
            let config = load_worker_config(workers)
                .expect("worker fixture should pass host-independent validation");
            let error = config
                .resolve_xdp(&BTreeSet::from([8, 9, 10]), Some(9))
                .expect_err("active explicit workers must not share the PoH core");
            assert_eq!(
                error.to_string(),
                "XDP worker CPU 9 overlaps the PoH core",
                "{case}"
            );
        }
    }

    #[test]
    fn test_unreferenced_explicit_workers_must_avoid_the_poh_core() {
        for (case, workers) in [
            ("cpus", "{ cpus = [8, 9] }"),
            (
                "bindings",
                "{ bindings = [{ queue = 3, cpu = 9 }, { queue = 0, cpu = 8 }] }",
            ),
        ] {
            let config = load_test_config(&format!(
                r#"
{ALL_COMPONENTS_QUEUE_ZERO}
[interfaces.primary]
device.name = "eth0"
xdp.zero_copy = false
xdp.workers = {workers}
"#
            ));
            let error = config
                .resolve_xdp(&BTreeSet::from([8, 9, 10]), Some(9))
                .expect_err("unreferenced explicit workers must not share the PoH core");
            assert_eq!(
                error.to_string(),
                "XDP worker CPU 9 overlaps the PoH core",
                "{case}"
            );
        }
    }

    #[test]
    fn test_auto_workers_require_enough_cpus_after_excluding_poh() {
        let config = load_worker_config("{ auto = { count = 2 } }")
            .expect("two-worker fixture should pass host-independent validation");
        let error = config
            .resolve_xdp(&BTreeSet::from([8, 9]), Some(9))
            .expect_err("auto workers must fail if too few CPUs remain after excluding PoH");
        assert_eq!(
            error.to_string(),
            "not enough CPUs for XDP: workers.auto.count = 2 needs 2 CPUs other than the PoH \
             core, but only 1 are available",
            "the error should report the requested worker count and available CPUs after \
             excluding PoH"
        );
    }

    #[test]
    fn test_auto_workers_use_the_highest_cpus_in_ascending_order() {
        let config = load_worker_config("{ auto = { count = 2 } }")
            .expect("two-worker fixture should pass host-independent validation");
        let runtime = config
            .resolve_xdp(&BTreeSet::from([7, 8, 9, 10, 11]), Some(11))
            .expect("auto workers should resolve on the highest CPUs other than PoH")
            .unwrap();
        assert_eq!(
            runtime.queues,
            [
                XdpWorkerBinding { queue: 0, cpu: 9 },
                XdpWorkerBinding { queue: 1, cpu: 10 },
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
                .resolve_xdp(&BTreeSet::from([0, 1]), None)
                .expect_err("active worker CPU must fit the Linux affinity mask");
            assert_eq!(error.kind(), DiagnosticKind::CpuResolution);
            assert!(
                error.message().contains("CPU 1000000 is outside"),
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
                .resolve_xdp(&BTreeSet::from([8, 10]), None)
                .expect("workers outside the process CPU affinity leave its CPUs unreserved")
                .unwrap();
            assert_eq!(
                runtime.queues,
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
            for (allowed, poh_core) in [
                (BTreeSet::from([8, 9]), None),
                (BTreeSet::from([7, 8, 9]), Some(7)),
            ] {
                let config = load_worker_config(workers).expect(
                    "worker fixture should pass validation before checking CPU reservations",
                );
                let error = config
                    .resolve_xdp(&allowed, poh_core)
                    .expect_err("active workers must leave at least one process CPU unreserved");
                assert!(
                    error.to_string().contains("leave at least one"),
                    "{workers} {poh_core:?}: {error}"
                );
            }
        }
    }

    #[test]
    fn test_empty_cpu_affinity_is_rejected() {
        let config = load_worker_config("{ cpus = [8] }")
            .expect("worker fixture should pass host-independent validation");
        let error = config
            .resolve_xdp(&BTreeSet::new(), None)
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
            .resolve_xdp(&BTreeSet::from([8, 9]), None)
            .expect("discarding the unreferenced worker should leave a CPU unreserved")
            .unwrap();
        assert_eq!(
            runtime.queues,
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
