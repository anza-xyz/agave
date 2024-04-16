# Agave Fuzzer

The agave fuzzer provides the API bindings for using `solfuzz` with Agave. It only works
on Linux operating systems.

Currently supported APIS:
 - sol_compat_instr_execute_v1

To build, use the `--release` flag for performance, the `--lib` flag to build a shared library and specify the 
target using the `--target` flag. The resulting file is a shared object in 
`target/x86_64-unknown-linux-gnu/libagave_fuzzer.so`.

```
cargo build --target x86_64-unknown-linux-gnu --release --lib
```

The resulting file is instrumented with sancov.

```
$ ldd target/x86_64-unknown-linux-gnu/release/libagave_fuzzer.so
        linux-vdso.so.1 (0x00007ffd0f9e3000)
        libgcc_s.so.1 => /lib/x86_64-linux-gnu/libgcc_s.so.1 (0x00007f6deb593000)
        libpthread.so.0 => /lib/x86_64-linux-gnu/libpthread.so.0 (0x00007f6deb570000)
        libm.so.6 => /lib/x86_64-linux-gnu/libm.so.6 (0x00007f6deb421000)
        libdl.so.2 => /lib/x86_64-linux-gnu/libdl.so.2 (0x00007f6deb41b000)
        libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f6deb229000)
        /lib64/ld-linux-x86-64.so.2 (0x00007f6dedc61000)

$ nm -D target/x86_64-unknown-linux-gnu/release/libagave_fuzzer.so | grep '__sanitizer'
        U __sanitizer_cov_8bit_counters_init
        U __sanitizer_cov_pcs_init
        U __sanitizer_cov_trace_cmp1
        U __sanitizer_cov_trace_cmp2
        U __sanitizer_cov_trace_cmp4
        U __sanitizer_cov_trace_cmp8
        U __sanitizer_cov_trace_const_cmp1
        U __sanitizer_cov_trace_const_cmp2
        U __sanitizer_cov_trace_const_cmp4
        U __sanitizer_cov_trace_const_cmp8
        U __sanitizer_cov_trace_pc_indir
        U __sanitizer_cov_trace_switch
```