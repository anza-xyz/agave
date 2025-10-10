#!/usr/bin/env bash

set -eo pipefail

source ci/_

cargo nextest run --profile ci  --manifest-path="svm/Cargo.toml" --features="shuttle-test" --test concurrent_tests --release --jobs 1
cargo nextest run --release --profile ci  --manifest-path="runtime/Cargo.toml" --features="shuttle-test" shuttle_tests
cargo nextest run --release --profile ci --manifest-path="poh/Cargo.toml" --features="shuttle-test" shuttle_tests
