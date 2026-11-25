//! Conformance fixtures emitted from harness runs.

use {
    crate::conformance::fd_hash::fd_hash_without_seed,
    prost::Message,
    protosol::protos::FixtureMetadata,
    std::{
        env, fs,
        marker::PhantomData,
        path::{Path, PathBuf},
    },
};

/// When set, each `FixtureWriter` writes its fixtures under `<dir>/<name>/`.
const EMIT_FIXTURES_ENV_VAR: &str = "EMIT_CONFORMANCE_FIXTURES";

/// A protosol fixture: the input a harness ran and the output it produced.
pub(crate) trait Fixture: Message + Sized {
    type Input;
    type Output;

    fn new(metadata: FixtureMetadata, input: Self::Input, output: Self::Output) -> Self;
}

macro_rules! impl_fixture {
    ($($fixture:ident($input:ident, $output:ident)),* $(,)?) => {
        $(
            impl Fixture for protosol::protos::$fixture {
                type Input = protosol::protos::$input;
                type Output = protosol::protos::$output;

                fn new(metadata: FixtureMetadata, input: Self::Input, output: Self::Output) -> Self {
                    Self {
                        metadata: Some(metadata),
                        input: Some(input),
                        output: Some(output),
                    }
                }
            }
        )*
    };
}

impl_fixture! {
    ElfLoaderFixture(ElfLoaderCtx, ElfLoaderEffects),
    InstrFixture(InstrContext, InstrEffects),
    SyscallFixture(SyscallContext, SyscallEffects),
    TxnFixture(TxnContext, TxnResult),
    VmSerializationFixture(InstrContext, VmSerializationEffects),
}

/// Writes the fixtures of type `F` that one harness produces.
///
/// The entrypoint lives on the writer rather than on `F` because one fixture
/// type can be replayed by several entrypoints (e.g. `TxnFixture`).
pub(crate) struct FixtureWriter<F> {
    /// The FFI entrypoint that replays the fixture.
    entrypoint: &'static str,
    /// The subdirectory this writer writes fixtures to.
    name: &'static str,
    _fixture: PhantomData<fn() -> F>,
}

impl<F> FixtureWriter<F> {
    pub(crate) const fn new(entrypoint: &'static str, name: &'static str) -> Self {
        Self {
            entrypoint,
            name,
            _fixture: PhantomData,
        }
    }
}

impl<F: Fixture> FixtureWriter<F> {
    /// Write a fixture if `EMIT_FIXTURES_ENV_VAR` is set. `build` only runs
    /// when it is, so harnesses pay for the conversions only when emitting.
    pub(crate) fn emit_if_requested(&self, build: impl FnOnce() -> (F::Input, F::Output)) {
        if let Some(directory) = env::var_os(EMIT_FIXTURES_ENV_VAR) {
            let (input, output) = build();
            self.write(Path::new(&directory), input, output);
        }
    }

    /// Write a fixture under `directory`, named by its hash so identical runs
    /// collapse into one file. Returns the path written.
    pub(crate) fn write(&self, directory: &Path, input: F::Input, output: F::Output) -> PathBuf {
        let metadata = FixtureMetadata {
            fn_entrypoint: self.entrypoint.to_string(),
        };
        let fixture = F::new(metadata, input, output).encode_to_vec();

        let directory = directory.join(self.name);
        fs::create_dir_all(&directory).expect("failed to create the fixture directory");
        let path = directory.join(format!("{:016x}.fix", fd_hash_without_seed(&fixture)));
        fs::write(&path, fixture).expect("failed to write the fixture");
        path
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        protosol::protos::{InstrContext, InstrEffects, InstrFixture},
        solana_pubkey::Pubkey,
    };

    const TEST_WRITER: FixtureWriter<InstrFixture> =
        FixtureWriter::new("sol_compat_test_v1", "test");

    #[test]
    fn test_write_round_trips() {
        let directory = env::temp_dir().join(format!("fixture-{}", Pubkey::new_unique()));
        let input = InstrContext {
            cu_avail: 42,
            ..InstrContext::default()
        };
        let output = InstrEffects {
            result: 1,
            ..InstrEffects::default()
        };

        let path = TEST_WRITER.write(&directory, input.clone(), output.clone());
        // Identical runs land in the same file.
        assert_eq!(
            TEST_WRITER.write(&directory, input.clone(), output.clone()),
            path
        );
        let bytes = fs::read(&path).unwrap();
        fs::remove_dir_all(&directory).unwrap();

        assert_eq!(path.parent().unwrap(), directory.join("test"));
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("{:016x}.fix", fd_hash_without_seed(&bytes)),
        );
        let fixture = InstrFixture::decode(bytes.as_slice()).unwrap();
        assert_eq!(
            fixture.metadata.unwrap().fn_entrypoint,
            "sol_compat_test_v1"
        );
        assert_eq!(fixture.input.unwrap(), input);
        assert_eq!(fixture.output.unwrap(), output);
    }
}
