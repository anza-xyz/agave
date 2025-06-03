use {min_max_heap::MinMaxHeap, solana_perf::packet::BytesPacket};

/// Container for storing packets and their priorities.
pub struct PacketContainer(MinMaxHeap<PacketContainerEntry>);

impl PacketContainer {
    pub fn with_capacity(capacity: usize) -> Self {
        Self(MinMaxHeap::with_capacity(capacity))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.0.len() == self.0.capacity()
    }

    pub fn min_priority(&self) -> Option<u64> {
        self.0.peek_min().map(|entry| entry.priority)
    }

    pub fn pop_max(&mut self) -> Option<BytesPacket> {
        self.0.pop_max().map(|entry| entry.packet)
    }

    pub fn pop_min(&mut self) -> Option<BytesPacket> {
        self.0.pop_min().map(|entry| entry.packet)
    }

    pub fn insert(&mut self, packet: BytesPacket, priority: u64) {
        self.0.push(PacketContainerEntry::new(packet, priority));
    }
}

#[derive(Eq, PartialEq)]
struct PacketContainerEntry {
    priority: u64,
    packet: BytesPacket,
}

impl Ord for PacketContainerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority)
    }
}

impl PartialOrd for PacketContainerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PacketContainerEntry {
    fn new(packet: BytesPacket, priority: u64) -> Self {
        Self { priority, packet }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_packet::PacketFlags};

    fn simple_packet_with_flags(packet_flags: PacketFlags) -> BytesPacket {
        let mut packet = BytesPacket::empty();
        packet.meta_mut().flags = packet_flags;
        packet
    }

    #[test]
    fn test_packet_container_status() {
        let mut container = PacketContainer::with_capacity(2);
        assert!(container.is_empty());
        assert!(!container.is_full());
        container.insert(simple_packet_with_flags(PacketFlags::empty()), 1);
        assert!(!container.is_empty());
        assert!(!container.is_full());
        container.insert(simple_packet_with_flags(PacketFlags::all()), 2);
        assert!(!container.is_empty());
        assert!(container.is_full());
    }

    #[test]
    fn test_packet_container_pop_min() {
        let mut container = PacketContainer::with_capacity(2);
        assert!(container.pop_min().is_none());
        container.insert(simple_packet_with_flags(PacketFlags::empty()), 1);
        container.insert(simple_packet_with_flags(PacketFlags::all()), 2);
        assert_eq!(
            container.pop_min().expect("not empty").meta().flags,
            PacketFlags::empty()
        );
        assert_eq!(
            container.pop_min().expect("not empty").meta().flags,
            PacketFlags::all()
        );
        assert!(container.pop_min().is_none());
    }

    #[test]
    fn test_packet_container_pop_max() {
        let mut container = PacketContainer::with_capacity(2);
        assert!(container.pop_max().is_none());
        container.insert(simple_packet_with_flags(PacketFlags::empty()), 1);
        container.insert(simple_packet_with_flags(PacketFlags::all()), 2);
        assert_eq!(
            container.pop_max().expect("not empty").meta().flags,
            PacketFlags::all()
        );
        assert_eq!(
            container.pop_max().expect("not empty").meta().flags,
            PacketFlags::empty()
        );
        assert!(container.pop_max().is_none());
    }
}
