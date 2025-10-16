#!/bin/bash

cargo +nightly-2025-02-16 clippy --workspace --all-targets --features dummy-for-ci-check,frozen-abi -- --deny=warnings --deny=clippy::default_trait_access --deny=clippy::arithmetic_side_effects --deny=clippy::manual_let_else --deny=clippy::used_underscore_binding
