use {crate::route::Route, std::net::Ipv4Addr};

const EMPTY_SLOT: u32 = u32::MAX;
const ROOT_NODE_INDEX: u32 = 0;
// these are for source code clarity, not because they can/should be changed
const NIBBLE_BITS: u8 = 4;
const IPV4_NIBBLES: usize = 8;
const CHILDREN_PER_NODE: usize = 16;

/// A longest-prefix-match lookup structure for IPv4 routes.
///
/// This is a nibble trie where each node has up to 16 children corresponding to
/// the 16 possible values of a 4-bit nibble.
#[derive(Clone, Debug)]
pub(crate) struct Ipv4Lpm {
    nodes: Vec<Node>,
    children: Vec<u32>,
}

impl Ipv4Lpm {
    /// Builds a lpm index from the given list of routes.
    ///
    /// If multiple routes share the same exact prefix, the first in the list wins. This means that
    /// route priority can be implemented by sorting the routes before calling this function.
    pub fn build(routes: &[Route<Ipv4Addr>]) -> Self {
        assert!(
            routes.len() < EMPTY_SLOT as usize,
            "too many routes to index with 32 bits"
        );
        let mut builder = Builder::new();
        for (route_idx, route) in routes.iter().enumerate() {
            builder.insert_route(route_idx as u32, route);
        }
        builder.finish()
    }

    /// Returns the index of the default route.
    pub fn default_route(&self) -> Option<u32> {
        self.nodes
            .get(ROOT_NODE_INDEX as usize)
            .and_then(|root| (root.value_idx != EMPTY_SLOT).then_some(root.value_idx))
    }

    /// Looks up the given address.
    ///
    /// Returns the index of the best matching route, or None if no route matches.
    pub fn lookup(&self, addr: Ipv4Addr) -> Option<u32> {
        let mut node = self.nodes.get(ROOT_NODE_INDEX as usize)?;
        let mut best = node.value_idx;
        let addr_bits = u32::from(addr);

        for depth in 0..IPV4_NIBBLES {
            let nibble = nibble_at(addr_bits, depth);
            // child_mask is a 16-bit bitfield where each bit corresponds to one of the 16 possible
            // nibbles/children. When a bit is set it means the child exists.
            let child_bit = 1u16 << nibble;
            if node.child_mask & child_bit == 0 {
                // no child for this nibble, stop
                break;
            }

            // children for this node are stored at children[child_base..child_base +
            // child_mask.count_ones()], which is to say the children are densely packed starting at
            // child_base, children[child_base + 1] is the second child, etc
            //
            // To find the index of the current nibble, we count how many bits are set in child_mask
            // that are to the left of child_bit.
            #[allow(clippy::arithmetic_side_effects)]
            let rank = (node.child_mask & (child_bit - 1)).count_ones();
            #[allow(clippy::arithmetic_side_effects)]
            let child_idx = self.children[(node.child_base + rank) as usize];

            node = &self.nodes[child_idx as usize];
            if node.value_idx != EMPTY_SLOT {
                best = node.value_idx;
            }
        }

        (best != EMPTY_SLOT).then_some(best)
    }
}

#[derive(Clone, Debug)]
struct Node {
    // index of the route to use if this node is a match, or EMPTY_SLOT if no route matches at this node
    value_idx: u32,
    // index into the children array where this node's children start
    child_base: u32,
    // bitmask indicating which children exist
    child_mask: u16,
}

// Builder for the Ipv4Lpm structure. This is used to incrementally build the trie before converting
// it into the compact format used for lookup.
#[derive(Clone)]
struct BuilderNode {
    value_idx: u32,
    value_prefix_len: u8,
    children: [u32; CHILDREN_PER_NODE],
}

impl BuilderNode {
    fn new() -> Self {
        Self {
            value_idx: EMPTY_SLOT,
            value_prefix_len: 0,
            children: [EMPTY_SLOT; CHILDREN_PER_NODE],
        }
    }
}

struct Builder {
    nodes: Vec<BuilderNode>,
}

impl Builder {
    fn new() -> Self {
        Self {
            nodes: vec![BuilderNode::new()],
        }
    }

    fn insert_route(&mut self, route_idx: u32, route: &Route<Ipv4Addr>) {
        let network_bits = match route.destination {
            None => 0,
            Some(addr) => u32::from(addr),
        };
        let prefix_len = route.dst_len.min(32);

        let full_nibbles = (prefix_len / NIBBLE_BITS) as usize;
        let partial_bits = prefix_len % NIBBLE_BITS;
        let mut node_idx = ROOT_NODE_INDEX;

        for depth in 0..full_nibbles {
            let nibble = nibble_at(network_bits, depth);
            node_idx = self.child_or_insert(node_idx, nibble);
        }

        if partial_bits == 0 {
            // The prefix ends on a nibble boundary, we can just set the value at this node
            self.set_value(node_idx, route_idx, prefix_len);
            return;
        }

        // We have a partial nibble at the end. We're going to insert multiple nodes to cover all
        // possible values of the bits after the prefix. Eg if the prefix is 17 bits, then we have 1
        // bit of the last nibble that is part of the prefix and 3 bits that are not. This means we
        // need to insert nodes for all 8 possible values of those 3 bits so at lookup we match the
        // prefix bits and then match any value for the remaining bits. This trades a bit more work
        // at build time for faster lookup.
        let base_nibble = nibble_at(network_bits, full_nibbles);
        let fanout_bits = NIBBLE_BITS.saturating_sub(partial_bits);
        let range_start = (base_nibble >> fanout_bits) << fanout_bits;
        let range_len = 1usize << fanout_bits;
        for nibble in range_start as usize..(range_start as usize).saturating_add(range_len) {
            let child_idx = self.child_or_insert(node_idx, nibble as u8);
            self.set_value(child_idx, route_idx, prefix_len);
        }
    }

    fn child_or_insert(&mut self, node_idx: u32, nibble: u8) -> u32 {
        let child_idx = self.nodes[node_idx as usize].children[nibble as usize];
        if child_idx != EMPTY_SLOT {
            return child_idx;
        }

        let child_idx = self.nodes.len() as u32;
        self.nodes.push(BuilderNode::new());
        self.nodes[node_idx as usize].children[nibble as usize] = child_idx;
        child_idx
    }

    fn set_value(&mut self, node_idx: u32, route_idx: u32, prefix_len: u8) {
        let node = &mut self.nodes[node_idx as usize];
        // If there's already a route at this node, only replace it if the new one has longer
        // prefix. This means that if there are multiple routes with the same prefix length, the
        // first one wins.
        if node.value_idx == EMPTY_SLOT || prefix_len > node.value_prefix_len {
            node.value_idx = route_idx;
            node.value_prefix_len = prefix_len;
        }
    }

    fn finish(self) -> Ipv4Lpm {
        let mut nodes = Vec::with_capacity(self.nodes.len());
        let mut children = Vec::new();

        for builder_node in self.nodes {
            let child_base = children.len() as u32;
            let mut child_mask = 0u16;

            for (nibble, child_idx) in builder_node.children.into_iter().enumerate() {
                if child_idx == EMPTY_SLOT {
                    continue;
                }
                child_mask |= 1u16 << nibble;
                children.push(child_idx);
            }

            // Convert from BuilderNode(s) where each node has a fixed array of 16 children, to the
            // compact format where children are densely packed and we have a bitmask to indicate
            // which children exist.
            nodes.push(Node {
                value_idx: builder_node.value_idx,
                child_base,
                child_mask,
            });
        }

        Ipv4Lpm { nodes, children }
    }
}

#[inline]
fn nibble_at(addr_bits: u32, depth: usize) -> u8 {
    // get the nibble at the given depth, where depth 0 is the most significant nibble
    // eg with depth=0 and addr_bits=0x12345678, this returns 0x1
    #[allow(clippy::arithmetic_side_effects)]
    let shift = 28u32 - (depth as u32) * u32::from(NIBBLE_BITS);
    ((addr_bits >> shift) & 0x0f) as u8
}

#[cfg(test)]
mod tests {
    use {super::*, crate::route::Route, std::net::Ipv4Addr};

    fn route(destination: Option<Ipv4Addr>, dst_len: u8) -> Route<Ipv4Addr> {
        Route {
            destination,
            gateway: None,
            preferred_src: None,
            out_if_index: Some(1),
            priority: None,
            type_: 0,
            dst_len,
        }
    }

    #[test]
    fn test_default_route() {
        let routes = vec![route(None, 0)];
        let lpm = Ipv4Lpm::build(&routes);

        assert_eq!(lpm.default_route(), Some(0));
        assert_eq!(lpm.lookup(Ipv4Addr::new(10, 0, 0, 0)), Some(0));
    }

    #[test]
    fn test_non_default_route() {
        let routes = vec![route(None, 0), route(Some(Ipv4Addr::new(10, 0, 0, 1)), 24)];
        let lpm = Ipv4Lpm::build(&routes);

        assert_eq!(lpm.lookup(Ipv4Addr::new(10, 0, 0, 1)), Some(1));
        assert_eq!(lpm.lookup(Ipv4Addr::new(10, 0, 1, 0)), Some(0));
    }

    #[test]
    fn test_longer_prefix_wins_order_descending() {
        let routes = vec![
            route(None, 0),
            route(Some(Ipv4Addr::new(10, 1, 1, 0)), 24),
            route(Some(Ipv4Addr::new(10, 1, 0, 0)), 16),
            route(Some(Ipv4Addr::new(10, 0, 0, 0)), 8),
        ];
        let lpm = Ipv4Lpm::build(&routes);

        assert_eq!(lpm.lookup(Ipv4Addr::new(10, 1, 1, 0)), Some(1));
        assert_eq!(lpm.lookup(Ipv4Addr::new(10, 1, 0, 0)), Some(2));
        assert_eq!(lpm.lookup(Ipv4Addr::new(10, 0, 0, 0)), Some(3));
        assert_eq!(lpm.lookup(Ipv4Addr::new(11, 0, 0, 0)), Some(0));
    }

    #[test]
    fn test_longer_prefix_wins_order_ascending() {
        let routes = vec![
            route(None, 0),
            route(Some(Ipv4Addr::new(10, 0, 0, 0)), 8),
            route(Some(Ipv4Addr::new(10, 1, 0, 0)), 16),
            route(Some(Ipv4Addr::new(10, 1, 1, 0)), 24),
        ];
        let lpm = Ipv4Lpm::build(&routes);

        assert_eq!(lpm.lookup(Ipv4Addr::new(10, 1, 1, 0)), Some(3));
        assert_eq!(lpm.lookup(Ipv4Addr::new(10, 1, 0, 0)), Some(2));
        assert_eq!(lpm.lookup(Ipv4Addr::new(10, 0, 0, 0)), Some(1));
        assert_eq!(lpm.lookup(Ipv4Addr::new(11, 0, 0, 0)), Some(0));
    }

    #[test]
    fn test_partial_nibble_prefixes() {
        let routes = vec![route(None, 0), route(Some(Ipv4Addr::new(10, 0, 0, 0)), 17)];
        let lpm = Ipv4Lpm::build(&routes);

        for i in 0x00..=0x7F {
            let addr = Ipv4Addr::new(10, 0, i, 0);
            assert_eq!(
                lpm.lookup(addr),
                Some(1),
                "addr {addr} should match the /17 route"
            );
        }
        assert_eq!(lpm.lookup(Ipv4Addr::new(10, 0, 0x80, 0)), Some(0));
    }

    #[test]
    fn test_equal_len_prefixes_first_wins() {
        let routes = vec![
            route(Some(Ipv4Addr::new(10, 0, 0, 0)), 8),
            route(Some(Ipv4Addr::new(10, 0, 0, 0)), 8),
        ];
        let lpm = Ipv4Lpm::build(&routes);

        assert_eq!(lpm.lookup(Ipv4Addr::new(10, 0, 0, 0)), Some(0));
    }
}
