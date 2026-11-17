use {
    crate::invoke_context::InvokeContext, solana_sbpf::program::BuiltinProgram,
    solana_svm_type_overrides::sync::Arc,
};

/// The registered functions (syscalls) and VM configuration (`Config`) that an
/// sBPF executable is verified, compiled, and executed against.
///
/// Compared and hashed by pointer identity, not by contents: environments built
/// separately are never equal, even if their functions and configuration match.
#[repr(transparent)]
#[derive(Clone, Debug)]
pub struct ProgramRuntimeEnvironment(Arc<BuiltinProgram<InvokeContext<'static, 'static>>>);

impl std::hash::Hash for ProgramRuntimeEnvironment {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::<BuiltinProgram<InvokeContext<'static, 'static>>>::as_ptr(&self.0).hash(state);
    }
}

impl PartialEq for ProgramRuntimeEnvironment {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ProgramRuntimeEnvironment {}

impl std::ops::Deref for ProgramRuntimeEnvironment {
    type Target = Arc<BuiltinProgram<InvokeContext<'static, 'static>>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ProgramRuntimeEnvironment {
    pub fn from(inner: BuiltinProgram<InvokeContext<'static, 'static>>) -> Self {
        Self(Arc::new(inner))
    }

    pub const fn from_ref<'a>(
        inner: &'a Arc<BuiltinProgram<InvokeContext<'static, 'static>>>,
    ) -> &'a Self {
        // Safety: This wrapper type is transparent and shares the same representation as the underlying type
        unsafe { std::mem::transmute(inner) }
    }
}

/// Paired execution and deployment environments.
///
/// Registered functions within each program runtime environment (syscalls)
/// depend on per-epoch feature gate statuses. In most cases, the list of
/// registered functions in the two environments will be the same. However,
/// it's possible that the effective epoch of deployment could be in the
/// *next epoch*.
///
/// SIMD-0688 (Single Program Runtime Environment) removes the need for a
/// separate deployment environment. Once it's active, this API can be cleaned
/// up.
pub struct ProgramRuntimeEnvironments {
    /// Environment compiled for the current epoch in which programs are
    /// executing.
    execution: ProgramRuntimeEnvironment,
    /// Environment compiled for the epoch of the next slot at which a program
    /// deployed in the current slot will execute.
    deployment: ProgramRuntimeEnvironment,
}

impl ProgramRuntimeEnvironments {
    /// Create a new ProgramRuntimeEnvironments from an `execution` and
    /// `deployment` environment.
    pub fn new(
        execution: ProgramRuntimeEnvironment,
        deployment: ProgramRuntimeEnvironment,
    ) -> Self {
        Self {
            execution,
            deployment,
        }
    }

    /// Get the program runtime environment for execution.
    pub fn get_env_for_execution(&self) -> &ProgramRuntimeEnvironment {
        &self.execution
    }

    /// Get the program runtime environment for deployment.
    pub fn get_env_for_deployment(&self) -> &ProgramRuntimeEnvironment {
        &self.deployment
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn mock() -> Self {
        Self {
            execution: get_mock_program_runtime_environment(),
            deployment: get_mock_program_runtime_environment(),
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
pub fn get_mock_program_runtime_environment() -> ProgramRuntimeEnvironment {
    static MOCK_ENVIRONMENT: std::sync::OnceLock<ProgramRuntimeEnvironment> =
        std::sync::OnceLock::<ProgramRuntimeEnvironment>::new();
    MOCK_ENVIRONMENT
        .get_or_init(|| ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock()))
        .clone()
}
