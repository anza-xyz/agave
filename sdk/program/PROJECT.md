## Future of the solana-program

The Solana Program SDK exists in the Agave validator monorepo. This has been
convenient for validator developers, but its presence in the monorepo has
sometimes hindered on-chain program developers. Since its release cycle is tied
to the Agave validator, it is not possible to easily release breaking changes.

The issue for on-chain developers has been brought up or debated in issues like:

* https://github.com/solana-labs/solana/issues/32503
* https://github.com/solana-labs/solana/issues/32625
* https://github.com/anza-xyz/agave/issues/20

To give on-chain program developers a better experience, it's time to define
some priorities and a plan to make things better.

## Concepts for the Program SDK

Here are the priorities for the future of the SDK, in order of importance.

1. Secure
2. Respect semantic versioning
3. On-chain program developers come first
4. Possible to build a new program quickly and easily
5. Sensible defaults, but allow customization
6. Minimal dependencies
7. Kitchen sinks only re-export functionality
8. Default to new crates or files to bundle functionality
9. If new functionality can't be done in a new crate or file, use features
10. Easy to publish
11. Easy for Agave devs to make changes

## Project Plan Phase 1: solana_program updates

### Break it up: if it can reasonably be used independently, it gets its own crate

As one approach to address point number 4 of easily allowing customization, we
should avoid bundling too much functionality in solana-program, and instead
break it up sensibly. This way, someone can easily drop in a custom entrypoint
without pulling in the default during build time.

This also helps address point number 8 of avoiding kitchen sinks. By putting
only related functionality into separate crates, there is no temptation for
developers to "just throw it into the SDK." Instead, they'll have to consider
the best place for their changes.

When breaking up solana-program, we will take it in phases. First, we'll extract
functionality required for program clients, which typically involve creating
instructions and deserializing types. Next, we will actually create program
client crates for the native programs. And finally, we will extract functionality
used for writing on-chain programs.

Here's a more specific breakdown:

* Add crates required for program clients:
  * borsh
  * clock
  * example-mocks
  * hash
  * instruction
  * message
  * program-error
  * pubkey
  * serde-varint
  * sysvar
* Those crates allow us to make program client crates:
  * address lookup table
  * bpf loader upgradeable
  * feature
  * loader v4
  * stake
  * system
  * vote
* Next, focus on crates for writing programs:
  * account-info
  * entrypoint
  * program-pack
  * program-syscalls
  * stable-layout
* Finally, do all the rest:
  * native-token
  * serialize-utils
  * wasm
* Deprecate any bits that make sense
* `solana_program` becomes a crate with only re-exports and deprecated code

### Add crate features

After solana-program has been broken up, the next bit is to address point number
7 and minimize dependencies. This will be done 

* Serialization: serde / borsh / bincode / bytemuck optional
* Syscalls: `#[cfg(not(target_os = "solana"))]` implementations of syscalls (ie.
`hashv`) pull in unneeded dependencies. Where possible, either gate these deps
with `[target.'cfg(not(target_os = "solana"))'.dependencies]` or create features
like `backup-syscall`
* Add an `unstable` feature for new code that could break, similar to Rust

### Update CI

* Reuse scripts and workflow for publishing crates:
  * `workflows/release.yml`
  * `publish-crate.sh`, minus dockerization
* Stop using Buildkite, use only GitHub Actions for these steps:
  * `test-sanity`
  * `shellcheck`
  * `test-checks`
  * `test-coverage`
  * `test-stable`
  * `test-wasm`
  * `test-docs`
* Add downstream testing (along with SPL and Anchor):
  * Agave monorepo clippy
  * Template programs
  * New program from scratch
* Backport
  * The new repo uses the same branching model as the monorepo, and contains
    a v2.0 branch, but not v1.18

### Move solana-program to a new repo

Note: this does not depend on the breakup of the crates, but probably makes more sense to do after

* Add a warning to Agave anytime a PR is opened modifying files that will be moved
* Create the monorepo
* Add scripts and instructions to agave and SPL to patch local solana-program crates

## Open Questions

* Does it make sense to make a new crate for every sysvar? Clock is useful separately, but what about rent, slot hashes, etc?
* Does it make sense to move ed25519 / keccak / secp256k1 to dedicated curve crates?

## Project Plan Phase 2: solana_sdk updates

Slightly different strategy: solana_sdk contains some modules used by downstream
devs and some used only by Agave, so we might need to keep this package in the
monorepo while deprecating the Agave-specific crates. The design for this will
come later.
