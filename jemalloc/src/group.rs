use {
    crate::jemalloc::{Arena, Decay, Error, Jemalloc},
    std::ops::Index,
};

#[derive(Debug)]
pub struct ArenaGroup {
    arenas: Box<[Arena]>,
}

impl ArenaGroup {
    pub fn new(
        arena_count: usize,
        oversize_threshold: usize,
        retain_grow_limit: usize,
    ) -> Result<Self, Error> {
        assert!(arena_count > 0, "arena count must be positive");

        let arenas = (0..arena_count)
            .map(|_| {
                let arena = Jemalloc::create_arena()?;
                arena.set_dirty_decay(Decay::Never)?;
                arena.set_muzzy_decay(Decay::Never)?;
                arena.set_oversize_threshold(oversize_threshold)?;
                arena.set_retain_grow_limit(retain_grow_limit)?;
                Ok(arena)
            })
            .collect::<Result<Vec<_>, Error>>()?
            .into_boxed_slice();

        Ok(Self { arenas })
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.arenas.len()
    }
}

impl Index<usize> for ArenaGroup {
    type Output = Arena;

    fn index(&self, index: usize) -> &Self::Output {
        &self.arenas[index]
    }
}
