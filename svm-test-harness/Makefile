
CARGO?=cargo

test: | binaries

binaries:
	$(CARGO) rustc --manifest-path ./Cargo.toml --lib --release --features fuzz --crate-type cdylib
	$(CARGO) build --manifest-path ./Cargo.toml --bins --release --features fuzz
