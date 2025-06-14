SBF_SDK_PATH := ../../platform-tools-sdk/sbf
SRC_DIR := c/src
OUT_DIR := target/deploy

clean-all: clean
	cargo clean

test:
	SBF_OUT_DIR=$(OUT_DIR) cargo test --features="sbf_rust,sbf_c" $(TEST_ARGS)

test-v0: all rust-v0 test

test-v1:
	VER=v1 $(MAKE) test-version

test-v2:
	VER=v2 $(MAKE) test-version

test-version:
	SBPF_CPU=$(VER) $(MAKE) all; \
	$(MAKE) rust-new ; \
	$(MAKE) test

rust-v0:
	cargo +solana build --release --target sbpf-solana-solana --workspace ; \
	cp -r target/sbpf-solana-solana/release/* target/deploy

rust-new:
	cargo +solana build --release --target sbpf$(VER)-solana-solana --workspace --features dynamic-frames ; \
	cp -r target/sbpf$(VER)-solana-solana/release/* target/deploy

.PHONY: rust-v0

include $(SBF_SDK_PATH)/c/sbf.mk
