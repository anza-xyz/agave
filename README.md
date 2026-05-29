<p align="center">
  <a href="https://anza.xyz">
    <img alt="Anza" src="https://i.postimg.cc/VkKTnMM9/agave-logo-talc-1.png" width="250" />
  </a>
</p>

[![Agave validator](https://img.shields.io/crates/v/agave-validator.svg)](https://crates.io/crates/agave-validator)
[![Agave documentation](https://docs.rs/agave-validator/badge.svg)](https://docs.rs/agave-validator)
[![Build status](https://badge.buildkite.com/b2b925facfdbb575573084bb4b7e1f1ce7f395239672941bf7.svg?branch=master)](https://buildkite.com/anza/agave-secondary)
[![Release status](https://github.com/anza-xyz/agave/actions/workflows/release.yml/badge.svg)](https://github.com/anza-xyz/agave/actions/workflows/release.yml)
[![codecov](https://codecov.io/gh/anza-xyz/agave/branch/master/graph/badge.svg)](https://codecov.io/gh/anza-xyz/agave)

# Building

## **1. Install rustc, cargo and rustfmt.**

```bash
$ curl https://sh.rustup.rs -sSf | sh
$ source $HOME/.cargo/env
$ rustup component add rustfmt
```

The `rust-toolchain.toml` file pins a specific rust version and ensures that
cargo commands run with that version. Note that cargo will automatically install
the correct version if it is not already installed.

On Linux systems you may need to install libssl-dev, pkg-config, zlib1g-dev, protobuf etc.

On Ubuntu:
```bash
$ sudo apt-get update
$ sudo apt-get install libssl-dev libudev-dev pkg-config zlib1g-dev llvm clang cmake make libprotobuf-dev protobuf-compiler libclang-dev
```

On Fedora:
```bash
$ sudo dnf install openssl-devel systemd-devel pkg-config zlib-devel llvm clang cmake make protobuf-devel protobuf-compiler perl-core libclang-dev
```

## **2. Download the source code.**

```bash
$ git clone https://github.com/anza-xyz/agave.git
$ cd agave
```

## **3. Build.**

```bash
$ ./cargo build
```

> [!NOTE]
> Note that this builds a debug version that is **not suitable for running a testnet or mainnet validator**. Please read [`docs/src/cli/install.md`](docs/src/cli/install.md#build-from-source) for instructions to build a release version for test and production uses.

# Testing

**Run the test suite:**

```bash
$ ./cargo nextest run --profile ci  --cargo-profile ci --config-file .config/nextest.toml
```

### Starting a local testnet

Start your own testnet locally, instructions are in the [online docs](https://docs.anza.xyz/clusters/benchmark).

### Accessing the remote development cluster

* `devnet` - stable public cluster for development accessible via
devnet.solana.com. Runs 24/7. Learn more about the [public clusters](https://docs.anza.xyz/clusters)

# Benchmarking

First, install the nightly build of rustc. `cargo bench` requires the use of the
unstable features only available in the nightly build.

```bash
$ rustup install nightly
```

Run the benchmarks:

```bash
$ cargo +nightly bench
```

# Release Process

The release process for this project is described [here](RELEASE.md).

# Code coverage

To generate code coverage statistics:

```bash
$ scripts/coverage.sh
$ open target/cov/lcov-local/index.html
```

Why coverage? While most see coverage as a code quality metric, we see it primarily as a developer
productivity metric. When a developer makes a change to the codebase, presumably it's a *solution* to
some problem.  Our unit-test suite is how we encode the set of *problems* the codebase solves. Running
the test suite should indicate that your change didn't *infringe* on anyone else's solutions. Adding a
test *protects* your solution from future changes. Say you don't understand why a line of code exists,
try deleting it and running the unit-tests. The nearest test failure should tell you what problem
was solved by that code. If no test fails, go ahead and submit a Pull Request that asks, "what
problem is solved by this code?" On the other hand, if a test does fail and you can think of a
better way to solve the same problem, a Pull Request with your solution would most certainly be
welcome! Likewise, if rewriting a test can better communicate what code it's protecting, please
send us that patch!

#########++++++++++++++++++

## RPC Batch Request Limits

The Agave RPC server accepts batch requests containing up to **450 individual RPC calls** per batch.

### Behavior

| Batch Size | HTTP Status | Result |
|------------|-------------|--------|
| 1 - 450 | 200 OK | ✅ Success - Returns array of responses |
| 500 or more | 413 Payload Too Large | ❌ Rejected - Entire batch fails |

### Important Notes

- Batches of **500 or more** requests are rejected with HTTP 413
- The server returns **no JSON error body** when rejecting large batches
- This limit applies to the **number of calls** in the batch, not the payload size

### Best Practices for Clients

To avoid errors when working with large batch operations:

```python
# Split large batches into chunks of 450 or fewer
def safe_batch_request(requests_list, chunk_size=450):
    results = []
    for i in range(0, len(requests_list), chunk_size):
        chunk = requests_list[i:i + chunk_size]
        response = requests.post(RPC_URL, json=chunk)
        if response.status_code == 200:
            results.extend(response.json())
        else:
            # Fall back to individual requests
            for req in chunk:
                single_response = requests.post(RPC_URL, json=req)
                results.append(single_response.json())
    return results
```

