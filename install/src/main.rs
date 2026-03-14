fn main() -> Result<(), String> {
    agave_instant::Instant::memoize();
    agave_install::main()
}
