use std::{
    cell::OnceCell,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::SystemTime,
};

pub const FLOW_ID: OnceCell<Arc<AtomicU64>> = OnceCell::new();

#[derive(Clone, Debug)]
pub struct FlowState {
    stream_created_at: SystemTime,
    first_chunk_received_at: Option<SystemTime>,
    flow_id: u64,
    signature: Option<String>,
}

impl FlowState {
    pub fn new(stream_created_at: SystemTime, first_chunk_received_at: Option<SystemTime>) -> Self {
        Self {
            stream_created_at,
            first_chunk_received_at,
            flow_id: Self::get_flow_id(),
        }
    }

    pub fn get_flow_id() -> u64 {
        FLOW_ID
            .get_or_init(|| Arc::new(AtomicU64::new(0)))
            .fetch_add(1, Ordering::Relaxed)
    }

    pub fn chunk_received(&mut self) {
        if self.first_chunk_received_at.is_none() {
            self.first_chunk_received_at = Some(SystemTime::now());
        }
    }

    pub fn set_signature(&mut self, signature: String) {
        self.signature = Some(signature);
    }
}

impl Eq for FlowState {}

impl PartialEq for FlowState {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Default for FlowState {
    fn default() -> Self {
        Self::new(SystemTime::now(), None)
    }
}
