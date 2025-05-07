SBF_SDK_PATH := ../../platform-tools-sdk/sbf
SRC_DIR := c/src
OUT_DIR := target/deploy

test-v3:
	mkdir -p target/deploy ; \
	VER=v3 $(MAKE) rust-new ; \
	$(MAKE) test-rust

test-v2:
	VER=v2 $(MAKE) test-version

test-v1:
	VER=v1 $(MAKE) test-version

test-v0: all rust-v0 test-all

clean-all:
	rm -rf target/deploy target/sbpf*

test-all:
	SBF_OUT_DIR=$(OUT_DIR) cargo test --features="sbf_rust,sbf_c" $(TEST_ARGS)

test-rust:
	SBF_OUT_DIR=$(OUT_DIR) cargo test --features="sbf_rust" $(TEST_ARGS)

test-version:
	SBPF_CPU=$(VER) $(MAKE) all ; \
	$(MAKE) rust-new ; \
	$(MAKE) test-all

rust-v0:
	cargo +solana build --release --target sbpf-solana-solana --workspace ; \
	cp -r target/sbpf-solana-solana/release/* target/deploy

rust-new:
	RUSTFLAGS="-C instrument-coverage=no" cargo +solana build --release --target sbpf$(VER)-solana-solana --workspace --features dynamic-frames ; \
	cp -r target/sbpf$(VER)-solana-solana/release/* target/deploy

.PHONY: test-v3

include $(SBF_SDK_PATH)/c/sbf.mk
