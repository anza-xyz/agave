use {
    crate::policy::{apply_policy, CoreAllocation},
    anyhow::bail,
    log::error,
    serde::{Deserialize, Serialize},
    solana_metrics::datapoint_info,
    std::{
        ops::Deref,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct NativeConfig {
    pub core_allocation: CoreAllocation,
    pub max_threads: usize,
    pub priority: u8,
    pub stack_size_bytes: usize,
}

impl Default for NativeConfig {
    fn default() -> Self {
        Self {
            core_allocation: CoreAllocation::OsDefault,
            max_threads: 16,
            priority: 0,
            stack_size_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct NativeThreadRuntimeInner {
    pub id_count: AtomicUsize,
    pub running_count: Arc<AtomicUsize>,
    pub config: NativeConfig,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct NativeThreadRuntime {
    inner: Arc<NativeThreadRuntimeInner>,
}

impl Deref for NativeThreadRuntime {
    type Target = NativeThreadRuntimeInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

pub struct JoinHandle<T> {
    std_handle: Option<std::thread::JoinHandle<T>>,
    running_count: Arc<AtomicUsize>,
}

impl<T> JoinHandle<T> {
    fn join_inner(&mut self) -> std::thread::Result<T> {
        match self.std_handle.take() {
            Some(jh) => {
                let result = jh.join();
                let rc = self.running_count.fetch_sub(1, Ordering::Relaxed);
                datapoint_info!("thread-manager-native", ("threads-running", rc, i64),);
                result
            }
            None => {
                panic!("Thread already joined");
            }
        }
    }

    pub fn join(mut self) -> std::thread::Result<T> {
        self.join_inner()
    }

    pub fn is_finished(&self) -> bool {
        match self.std_handle {
            Some(ref jh) => jh.is_finished(),
            None => true,
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if self.std_handle.is_some() {
            error!("Attempting to drop a Join Handle of a running thread will leak thread IDs, please join your managed threads!");
            self.join_inner().expect("Child thread panicked");
        }
    }
}

impl NativeThreadRuntime {
    pub fn new(name: String, cfg: NativeConfig) -> Self {
        Self {
            inner: Arc::new(NativeThreadRuntimeInner {
                id_count: AtomicUsize::new(0),
                running_count: Arc::new(AtomicUsize::new(0)),
                config: cfg,
                name,
            }),
        }
    }

    pub fn spawn<F, T>(&self, f: F) -> anyhow::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T,
        F: Send + 'static,
        T: Send + 'static,
    {
        let n = self.id_count.fetch_add(1, Ordering::Relaxed);
        let name = format!("{}-{}", &self.name, n);
        self.spawn_named(name, f)
    }

    pub fn spawn_named<F, T>(&self, name: String, f: F) -> anyhow::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T,
        F: Send + 'static,
        T: Send + 'static,
    {
        let spawned = self.running_count.load(Ordering::Relaxed);
        if spawned >= self.config.max_threads {
            bail!("All allowed threads in this pool are already spawned");
        }

        let core_alloc = self.config.core_allocation.clone();
        let priority = self.config.priority;
        let chosen_cores_mask = Mutex::new(self.config.core_allocation.as_core_mask_vector());
        let jh = std::thread::Builder::new()
            .name(name)
            .stack_size(self.config.stack_size_bytes)
            .spawn(move || {
                apply_policy(&core_alloc, priority, &chosen_cores_mask);
                f()
            })?;
        let rc = self.running_count.fetch_add(1, Ordering::Relaxed);
        datapoint_info!("thread-manager-native", ("threads-running", rc as i64, i64),);
        Ok(JoinHandle {
            std_handle: Some(jh),
            running_count: self.running_count.clone(),
        })
    }
}
