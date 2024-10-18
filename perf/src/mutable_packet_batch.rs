use {crate::packet::PacketBatch, core::borrow::Borrow, std::sync::Arc};

/// Helper trait to make it simple to work with:
/// - `&mut [PacketBatch]` or
/// - `&mut [Arc<PacketBatch>]`
pub trait MutablePacketBatch: Borrow<PacketBatch> + Send {
    fn as_mut(&mut self) -> &mut PacketBatch;
}

impl MutablePacketBatch for PacketBatch {
    fn as_mut(&mut self) -> &mut PacketBatch {
        self
    }
}

impl MutablePacketBatch for Arc<PacketBatch> {
    fn as_mut(&mut self) -> &mut PacketBatch {
        Arc::make_mut(self)
    }
}
