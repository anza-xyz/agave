use crate::{recycled_vec::RecycledVec, recycler::Recycler, sigverify::TxOffset};

#[derive(Default, Clone)]
pub struct RecyclerCache {
    recycler_offsets: Recycler<TxOffset>,
    recycler_buffer: Recycler<RecycledVec<u8>>,
}

impl RecyclerCache {
    pub fn warmed() -> Self {
        Self {
            recycler_offsets: Recycler::new(),
            recycler_buffer: Recycler::new(),
        }
    }
    pub fn offsets(&self) -> &Recycler<TxOffset> {
        &self.recycler_offsets
    }
    pub fn buffer(&self) -> &Recycler<RecycledVec<u8>> {
        &self.recycler_buffer
    }
}
