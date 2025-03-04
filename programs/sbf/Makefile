SBF_SDK_PATH := ../../platform-tools-sdk/sbf
SRC_DIR := c/src
OUT_DIR := target/deploy

clean-all: clean
	cargo clean

test:
	SBF_OUT_DIR=$(OUT_DIR) cargo test --features="sbf_rust,sbf_c" $(TEST_ARGS)

test-v1:
	SBPF_CPU=v1 $(MAKE) all ; \
	$(MAKE) rust-v1 ; \
	$(MAKE) test

test-v0: all rust-v0 test

rust-v0:
	cargo +solana build --release --target sbpf-solana-solana --workspace ; \
	cp -r target/sbpf-solana-solana/release/* target/deploy

rust-v1:
	cargo +solana build --release --target sbpfv1-solana-solana --workspace --features dynamic-frames ; \
	cp -r target/sbpfv1-solana-solana/release/* target/deploy

.PHONY: rust-v0

include $(SBF_SDK_PATH)/c/sbf.mk
