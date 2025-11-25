use {
    crate::shred::Error, solana_hash::Hash, solana_sha256_hasher::hashv,
    static_assertions::const_assert_eq, std::iter::successors,
};

pub(crate) const SIZE_OF_MERKLE_ROOT: usize = std::mem::size_of::<Hash>();
const_assert_eq!(SIZE_OF_MERKLE_ROOT, 32);
const_assert_eq!(SIZE_OF_MERKLE_PROOF_ENTRY, 20);
pub(crate) const SIZE_OF_MERKLE_PROOF_ENTRY: usize = std::mem::size_of::<MerkleProofEntry>();
// Number of proof entries for the standard 64 shred batch.
pub(crate) const PROOF_ENTRIES_FOR_32_32_BATCH: u8 = 6;

// Defense against second preimage attack:
// https://en.wikipedia.org/wiki/Merkle_tree#Second_preimage_attack
// Following Certificate Transparency, 0x00 and 0x01 bytes are prepended to
// hash data when computing leaf and internal node hashes respectively.
pub(crate) const MERKLE_HASH_PREFIX_LEAF: &[u8] = b"\x00SOLANA_MERKLE_SHREDS_LEAF";
pub(crate) const MERKLE_HASH_PREFIX_NODE: &[u8] = b"\x01SOLANA_MERKLE_SHREDS_NODE";

pub(crate) type MerkleProofEntry = [u8; 20];

/// A struct to track a given Merkle tree.
pub(crate) struct MerkleTree {
    /// List of all the nodes in the tree.
    /// The constructor ensures that this is not empty.
    /// The last node in the list is the root of the tree.
    nodes: Vec<Hash>,
}

impl MerkleTree {
    /// Tries to constructs a `MerkleTree`.
    ///
    /// # Errors:
    ///
    /// [`Error::EmptyIterator`] if the function is called with an empty iterator.
    /// [`Error`] if any of the elements in the iterator contains an [`Error`].
    pub(crate) fn try_new(
        shreds: impl ExactSizeIterator<Item = Result<Hash, Error>>,
    ) -> Result<MerkleTree, Error> {
        if shreds.len() == 0 {
            return Err(Error::EmptyIterator);
        }
        let num_shreds = shreds.len();
        let capacity = get_merkle_tree_size(num_shreds);
        let mut nodes = Vec::with_capacity(capacity);
        for shred in shreds {
            nodes.push(shred?);
        }
        let init = (num_shreds > 1).then_some(num_shreds);
        for size in successors(init, |&k| (k > 2).then_some((k + 1) >> 1)) {
            let offset = nodes.len() - size;
            for index in (offset..offset + size).step_by(2) {
                let node = &nodes[index];
                let other = &nodes[(index + 1).min(offset + size - 1)];
                let parent = join_nodes(node, other);
                nodes.push(parent);
            }
        }
        debug_assert_eq!(nodes.len(), capacity);
        Ok(MerkleTree { nodes })
    }

    /// Returns a reference to the root of the tree.
    pub(crate) fn root(&self) -> &Hash {
        // constructor ensures that the tree contains at least one node so this unwrap() should be safe.
        self.nodes.last().unwrap()
    }

    pub(crate) fn make_merkle_proof(
        &self,
        mut index: usize, // leaf index ~ shred's erasure shard index.
        mut size: usize,  // number of leaves ~ erasure batch size.
    ) -> impl Iterator<Item = Result<&MerkleProofEntry, Error>> {
        let mut offset = 0;
        if index >= size {
            // Force below iterator to return Error.
            (size, offset) = (0, self.nodes.len());
        }
        std::iter::from_fn(move || {
            if size > 1 {
                let Some(node) = self.nodes.get(offset + (index ^ 1).min(size - 1)) else {
                    return Some(Err(Error::InvalidMerkleProof));
                };
                offset += size;
                size = (size + 1) >> 1;
                index >>= 1;
                let entry = &node.as_ref()[..SIZE_OF_MERKLE_PROOF_ENTRY];
                let entry = <&MerkleProofEntry>::try_from(entry).unwrap();
                Some(Ok(entry))
            } else if offset + 1 == self.nodes.len() {
                None
            } else {
                Some(Err(Error::InvalidMerkleProof))
            }
        })
    }
}

// Obtains parent's hash by joining two sibling nodes in merkle tree.
fn join_nodes<S: AsRef<[u8]>, T: AsRef<[u8]>>(node: S, other: T) -> Hash {
    let node = &node.as_ref()[..SIZE_OF_MERKLE_PROOF_ENTRY];
    let other = &other.as_ref()[..SIZE_OF_MERKLE_PROOF_ENTRY];
    hashv(&[MERKLE_HASH_PREFIX_NODE, node, other])
}

// Recovers root of the merkle tree from a leaf node
// at the given index and the respective proof.
pub fn get_merkle_root<'a, I>(index: usize, node: Hash, proof: I) -> Result<Hash, Error>
where
    I: IntoIterator<Item = &'a MerkleProofEntry>,
{
    let (index, root) = proof
        .into_iter()
        .fold((index, node), |(index, node), other| {
            let parent = if index % 2 == 0 {
                join_nodes(node, other)
            } else {
                join_nodes(other, node)
            };
            (index >> 1, parent)
        });
    (index == 0)
        .then_some(root)
        .ok_or(Error::InvalidMerkleProof)
}

// Given number of shreds, returns the number of nodes in the Merkle tree.
pub fn get_merkle_tree_size(num_shreds: usize) -> usize {
    successors(Some(num_shreds), |&k| (k > 1).then_some((k + 1) >> 1)).sum()
}

// Maps number of (code + data) shreds to merkle_proof.len().
#[cfg(test)]
pub(crate) const fn get_proof_size(num_shreds: usize) -> u8 {
    let bits = usize::BITS - num_shreds.leading_zeros();
    let proof_size = if num_shreds.is_power_of_two() {
        bits.saturating_sub(1)
    } else {
        bits
    };
    // this can never overflow because bits < 64
    proof_size as u8
}

#[cfg(test)]
mod tests {
    use {
        crate::shred::merkle_tree::*, assert_matches::assert_matches, rand::Rng,
        std::iter::repeat_with,
    };

    #[test]
    fn test_merkle_proof_entry_from_hash() {
        let mut rng = rand::rng();
        let bytes: [u8; 32] = rng.random();
        let hash = Hash::from(bytes);
        let entry = &hash.as_ref()[..SIZE_OF_MERKLE_PROOF_ENTRY];
        let entry = MerkleProofEntry::try_from(entry).unwrap();
        assert_eq!(entry, &bytes[..SIZE_OF_MERKLE_PROOF_ENTRY]);
    }

    #[test]
    fn test_get_merkle_tree_size() {
        const TREE_SIZE: [usize; 15] = [0, 1, 3, 6, 7, 11, 12, 14, 15, 20, 21, 23, 24, 27, 28];
        for (num_shreds, size) in TREE_SIZE.into_iter().enumerate() {
            assert_eq!(get_merkle_tree_size(num_shreds), size);
        }
    }

    #[test]
    fn test_make_merkle_proof_error() {
        let mut rng = rand::rng();
        let nodes = repeat_with(|| rng.random::<[u8; 32]>()).map(Hash::from);
        let nodes: Vec<_> = nodes.take(5).collect();
        let size = nodes.len();
        let tree = MerkleTree::try_new(nodes.into_iter().map(Ok)).unwrap();
        for index in size..size + 3 {
            assert_matches!(
                tree.make_merkle_proof(index, size).next(),
                Some(Err(Error::InvalidMerkleProof))
            );
        }
    }

    fn run_merkle_tree_round_trip<R: Rng>(rng: &mut R, size: usize) {
        let nodes = repeat_with(|| rng.random::<[u8; 32]>()).map(Hash::from);
        let nodes: Vec<_> = nodes.take(size).collect();
        let tree = MerkleTree::try_new(nodes.iter().cloned().map(Ok)).unwrap();
        let root = *tree.root();
        for index in 0..size {
            for (k, &node) in nodes.iter().enumerate() {
                let proof = tree.make_merkle_proof(index, size).map(Result::unwrap);
                if k == index {
                    assert_eq!(root, get_merkle_root(k, node, proof).unwrap());
                } else {
                    assert_ne!(root, get_merkle_root(k, node, proof).unwrap());
                }
            }
        }
    }

    #[test]
    fn test_merkle_tree_round_trip_small() {
        let mut rng = rand::rng();
        for size in 1..=110 {
            run_merkle_tree_round_trip(&mut rng, size);
        }
    }

    #[test]
    fn test_merkle_tree_round_trip_big() {
        let mut rng = rand::rng();
        for size in 110..=143 {
            run_merkle_tree_round_trip(&mut rng, size);
        }
    }

    #[test]
    fn test_get_proof_size() {
        assert_eq!(get_proof_size(0), 0);
        assert_eq!(get_proof_size(1), 0);
        assert_eq!(get_proof_size(2), 1);
        assert_eq!(get_proof_size(3), 2);
        assert_eq!(get_proof_size(4), 2);
        assert_eq!(get_proof_size(5), 3);
        assert_eq!(get_proof_size(63), 6);
        assert_eq!(get_proof_size(64), 6);
        assert_eq!(get_proof_size(65), 7);
        assert_eq!(get_proof_size(usize::MAX - 1), 64);
        assert_eq!(get_proof_size(usize::MAX), 64);
        for proof_size in 1u8..9 {
            let max_num_shreds = 1usize << u32::from(proof_size);
            let min_num_shreds = (max_num_shreds >> 1) + 1;
            for num_shreds in min_num_shreds..=max_num_shreds {
                assert_eq!(get_proof_size(num_shreds), proof_size);
            }
        }
    }
}
