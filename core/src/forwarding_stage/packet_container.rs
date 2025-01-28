use {min_max_heap::MinMaxHeap, slab::Slab, solana_perf::packet::Packet};

/// Container for storing packets.
/// Packet IDs are stored with priority in a priority queue and the actual
/// `Packet` are stored in a map.
pub struct PacketContainer {
    priority_queue: MinMaxHeap<PriorityIndex>,
    packets: Slab<Packet>,
}

impl PacketContainer {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            priority_queue: MinMaxHeap::with_capacity(capacity),
            packets: Slab::with_capacity(capacity),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.priority_queue.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.priority_queue.len() == self.priority_queue.capacity()
    }

    pub fn min_priority(&self) -> Option<u64> {
        self.priority_queue.peek_min().map(|min| min.priority)
    }

    pub fn pop_and_remove_max(&mut self) -> Option<Packet> {
        self.priority_queue
            .pop_max()
            .map(|max| self.packets.remove(max.index))
    }

    pub fn pop_and_remove_min(&mut self) -> Option<Packet> {
        self.priority_queue
            .pop_min()
            .map(|min| self.packets.remove(min.index))
    }

    pub fn insert(&mut self, packet: Packet, priority: u64) {
        let entry = self.packets.vacant_entry();
        let index = entry.key();
        entry.insert(packet.clone());
        let priority_index = PriorityIndex { priority, index };
        self.priority_queue.push(priority_index);
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
struct PriorityIndex {
    priority: u64,
    index: usize,
}

#[cfg(test)]
mod tests {
    use {super::*, solana_sdk::packet::PacketFlags};

    fn simple_packet_with_flags(packet_flags: PacketFlags) -> Packet {
        let mut packet = Packet::default();
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
    fn test_packet_container_pop_and_remove_min() {
        let mut container = PacketContainer::with_capacity(2);
        assert!(container.pop_and_remove_min().is_none());
        container.insert(simple_packet_with_flags(PacketFlags::empty()), 1);
        container.insert(simple_packet_with_flags(PacketFlags::all()), 2);
        assert_eq!(
            container
                .pop_and_remove_min()
                .expect("not empty")
                .meta()
                .flags,
            PacketFlags::empty()
        );
        assert_eq!(
            container
                .pop_and_remove_min()
                .expect("not empty")
                .meta()
                .flags,
            PacketFlags::all()
        );
        assert!(container.pop_and_remove_min().is_none());
    }

    #[test]
    fn test_packet_container_pop_and_remove_max() {
        let mut container = PacketContainer::with_capacity(2);
        assert!(container.pop_and_remove_max().is_none());
        container.insert(simple_packet_with_flags(PacketFlags::empty()), 1);
        container.insert(simple_packet_with_flags(PacketFlags::all()), 2);
        assert_eq!(
            container
                .pop_and_remove_max()
                .expect("not empty")
                .meta()
                .flags,
            PacketFlags::all()
        );
        assert_eq!(
            container
                .pop_and_remove_max()
                .expect("not empty")
                .meta()
                .flags,
            PacketFlags::empty()
        );
        assert!(container.pop_and_remove_max().is_none());
    }
}
