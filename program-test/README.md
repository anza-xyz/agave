# Program test

TODO: Describe the intent of this crate.

## How to add a new program

1. Compile the program for SBF target.

For, example, to compile the `noop` program from `programs/sbf/rust/noop` you need to do the
following:

```bash
cd programs/sbf/rust/noop
../../../../cargo-build-sbf

# Go to programs/sbf
cd ../..
# Program binary will be in `target/sbf-solana-solana/release`:
ls target/sbf-solana-solana/release/solana_sbf_rust_noop.so
```

2. Reduce the binary size, using `strip.sh` from the SDK.

```bash
# From the workspace root.
sdk/sbf/scripts/strip.sh \
    programs/sbf/target/sbf-solana-solana/release/solana_sbf_rust_noop.so \
    program-test/src/programs/noop.so
```

3. Add the binary to the list of available programs.

TODO: Describe the steps.
