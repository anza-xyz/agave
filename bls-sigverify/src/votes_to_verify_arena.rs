use crate::bls_vote_sigverify::UnverifiedVotePayload;

#[derive(Default)]
pub(crate) struct VotesToVerifyArena {
    arena: Vec<Vec<UnverifiedVotePayload>>,
}

impl VotesToVerifyArena {
    pub(crate) fn alloc_batch(&mut self) -> Vec<UnverifiedVotePayload> {
        self.arena.pop().unwrap_or_default()
    }

    pub(crate) fn return_batch(&mut self, mut batch: Vec<UnverifiedVotePayload>) {
        batch.clear();
        self.arena.push(batch);
    }
}
