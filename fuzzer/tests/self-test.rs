#[test]
fn sanity_test() {
    #[cfg(target_os = "linux")]
    {
        use std::process::Command;

        #[cfg(target_arch = "x86_64")]
        let name = "x86_64-unknown-linux-gnu";

        #[cfg(target_arch = "aarch64")]
        let name = "aarch64-unknown-linux-gnu";

        let folder = "target/".to_owned() + name + "/release/libagave_fuzzer.so";
        let output = Command::new("target/self_test")
            .arg(folder)
            .output()
            .expect("Test failed");
        assert_eq!(output.stderr.as_slice(), "OK\n".as_bytes());
    }
}
