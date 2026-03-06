#![cfg(feature = "agave-unstable-api")]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
use {
    agave_feature_set::{FeatureSet, enable_secp256r1_precompile},
    solana_message::compiled_instruction::CompiledInstruction,
    solana_precompile_error::PrecompileError,
    solana_pubkey::Pubkey,
    std::sync::LazyLock,
};

pub mod ed25519;
pub mod secp256k1;
pub mod secp256r1;

/// All precompiled programs must implement the `Verify` function
pub type Verify = fn(&[u8], &[&[u8]], &FeatureSet) -> std::result::Result<(), PrecompileError>;

/// Information on a precompiled program
pub struct Precompile {
    /// Program id
    pub program_id: Pubkey,
    /// Feature to enable on, `None` indicates always enabled
    pub feature: Option<Pubkey>,
    /// Verification function
    pub verify_fn: Verify,
}
impl Precompile {
    /// Creates a new `Precompile`
    pub fn new(program_id: Pubkey, feature: Option<Pubkey>, verify_fn: Verify) -> Self {
        Precompile {
            program_id,
            feature,
            verify_fn,
        }
    }
    /// Check if a program id is this precompiled program
    pub fn check_id<F>(&self, program_id: &Pubkey, is_enabled: F) -> bool
    where
        F: Fn(&Pubkey) -> bool,
    {
        self.feature
            .is_none_or(|ref feature_id| is_enabled(feature_id))
            && self.program_id == *program_id
    }
    /// Verify this precompiled program
    pub fn verify(
        &self,
        data: &[u8],
        instruction_datas: &[&[u8]],
        feature_set: &FeatureSet,
    ) -> std::result::Result<(), PrecompileError> {
        (self.verify_fn)(data, instruction_datas, feature_set)
    }
}

/// The list of all precompiled programs
static PRECOMPILES: LazyLock<Vec<Precompile>> = LazyLock::new(|| {
    vec![
        Precompile::new(
            solana_sdk_ids::secp256k1_program::id(),
            None, // always enabled
            secp256k1::verify,
        ),
        Precompile::new(
            solana_sdk_ids::ed25519_program::id(),
            None, // always enabled
            ed25519::verify,
        ),
        Precompile::new(
            solana_sdk_ids::secp256r1_program::id(),
            Some(enable_secp256r1_precompile::id()),
            secp256r1::verify,
        ),
    ]
});

/// Check if a program is a precompiled program
pub fn is_precompile<F>(program_id: &Pubkey, is_enabled: F) -> bool
where
    F: Fn(&Pubkey) -> bool,
{
    PRECOMPILES
        .iter()
        .any(|precompile| precompile.check_id(program_id, |feature_id| is_enabled(feature_id)))
}

/// Find an enabled precompiled program
pub fn get_precompile<F>(program_id: &Pubkey, is_enabled: F) -> Option<&Precompile>
where
    F: Fn(&Pubkey) -> bool,
{
    PRECOMPILES
        .iter()
        .find(|precompile| precompile.check_id(program_id, |feature_id| is_enabled(feature_id)))
}

pub fn get_precompiles<'a>() -> &'a [Precompile] {
    &PRECOMPILES
}

/// Check that a program is precompiled and if so verify it
pub fn verify_if_precompile(
    program_id: &Pubkey,
    precompile_instruction: &CompiledInstruction,
    all_instructions: &[CompiledInstruction],
    feature_set: &FeatureSet,
) -> Result<(), PrecompileError> {
    for precompile in PRECOMPILES.iter() {
        if precompile.check_id(program_id, |feature_id| feature_set.is_active(feature_id)) {
            let instruction_datas: Vec<_> = all_instructions
                .iter()
                .map(|instruction| instruction.data.as_ref())
                .collect();
            return precompile.verify(
                &precompile_instruction.data,
                &instruction_datas,
                feature_set,
            );
        }
    }
    Ok(())
}

/// Test that `verify` produces consistent results regardless of memory alignment.
/// This catches implementations that unsafely cast byte slices to structured types.
#[cfg(test)]
pub(crate) fn test_verify_with_alignment(
    verify: Verify,
    instruction_data: &[u8],
    instruction_datas: &[&[u8]],
    feature_set: &FeatureSet,
) -> Result<(), PrecompileError> {
    // First, verify with original alignment.
    let result = verify(instruction_data, instruction_datas, feature_set);

    // Now test with shifted alignment to ensure verify doesn't depend on alignment.
    let mut misaligned_buf = vec![0u8; instruction_data.len() + 1];
    misaligned_buf[1..].copy_from_slice(instruction_data);
    let misaligned_data = &misaligned_buf[1..];

    // Substitute misaligned data in instruction_datas where it matches instruction_data.
    // This preserves pointer identity for precompiles that rely on it.
    let misaligned_instruction_datas: Vec<&[u8]> = instruction_datas
        .iter()
        .map(|&ix| {
            if std::ptr::eq(ix.as_ptr(), instruction_data.as_ptr()) {
                misaligned_data
            } else {
                ix
            }
        })
        .collect();

    let result_misaligned = verify(misaligned_data, &misaligned_instruction_datas, feature_set);
    assert_eq!(result, result_misaligned, "verify result differs with misaligned data");

    result
}
