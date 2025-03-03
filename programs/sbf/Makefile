SBF_SDK_PATH := ../../platform-tools-sdk/sbf
SRC_DIR := c/src
OUT_DIR := target/deploy

test-v0: all rust-v0
	SBF_OUT_DIR=$(OUT_DIR) cargo test --features="sbf_rust,sbf_c" $(TEST_ARGS)

rust-v0:
	cargo +solana build --release --target sbpf-solana-solana --workspace ; \
	cp -r target/sbpf-solana-solana/release/* target/deploy

.PHONY: rust

include $(SBF_SDK_PATH)/c/sbf.mk
