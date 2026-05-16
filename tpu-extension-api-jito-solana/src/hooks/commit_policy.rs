use agave_tpu_extension_api::{BatchCommitMode, BatchCommitPolicy};

pub struct BundleCommitPolicy;

impl BatchCommitPolicy for BundleCommitPolicy {
    #[inline(always)]
    fn mode(&self) -> BatchCommitMode {
        BatchCommitMode::all_or_nothing()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_commit_semantics() {
        assert!(BundleCommitPolicy.mode().reverts_on_error());
    }
}
