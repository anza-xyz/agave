
CARGO?=cargo

test: | binaries tests/run_test_vectors

binaries:
	$(CARGO) build --manifest-path ./Cargo.toml --bins --release

tests/run_test_vectors:
	scripts/run_test_vectors.sh
