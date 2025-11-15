#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
pub mod data_budget;
pub mod deduper;
pub mod discard;
pub mod packet;
pub mod perf_libs;
pub mod recycled_vec;
pub mod recycler;
pub mod recycler_cache;
pub mod sigverify;
#[cfg(feature = "dev-context-only-utils")]
pub mod test_tx;
pub mod thread;

#[macro_use]
extern crate log;

#[cfg(test)]
#[macro_use]
extern crate assert_matches;

#[macro_use]
extern crate solana_metrics;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;

fn is_rosetta_emulated() -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::str::FromStr;
        std::process::Command::new("sysctl")
            .args(["-in", "sysctl.proc_translated"])
            .output()
            .map_err(|_| ())
            .and_then(|output| String::from_utf8(output.stdout).map_err(|_| ()))
            .and_then(|stdout| u8::from_str(stdout.trim()).map_err(|_| ()))
            .map(|enabled| enabled == 1)
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

pub fn report_target_features() {
    // Validator binaries built on a machine with AVX support will generate invalid opcodes
    // when run on machines without AVX causing a non-obvious process abort.  Instead detect
    // the mismatch and error cleanly.
    if !is_rosetta_emulated() {
        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            build_target_feature_avx
        ))]
        {
            if is_x86_feature_detected!("avx") {
                info!("AVX detected");
            } else {
                error!(
                    "Incompatible CPU detected: missing AVX support. Please build from source on \
                     the target"
                );
                std::process::abort();
            }
        }

        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            build_target_feature_avx2
        ))]
        {
            if is_x86_feature_detected!("avx2") {
                info!("AVX2 detected");
            } else {
                error!(
                    "Incompatible CPU detected: missing AVX2 support. Please build from source on \
                     the target"
                );
                std::process::abort();
            }
        }
    }
}
