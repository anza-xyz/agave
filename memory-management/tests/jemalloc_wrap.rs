#![cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
use {solana_memory_management::jemalloc_monitor::*, std::time::Duration};

#[global_allocator]
static GLOBAL_ALLOC_WRAP: JemWrapAllocator = JemWrapAllocator::new();

pub fn print_allocations() {
    view_allocations(|stats| {
        println!("allocated so far: {:?}", stats);
    });
}

// This is not really a test as such, more a canary to check if the logic works and does not deadlock.
// None of the reported data is "exact science"
fn main() {
    let mut mps = MemPoolStats::default();
    mps.add("Foo");
    mps.add("Boo");
    init_allocator(mps);

    let _s = "allocating a string!".to_owned();
    print_allocations();

    let jh1 = std::thread::Builder::new()
        .name("Foo thread 1".to_string())
        .spawn(|| {
            let _s2 = "allocating a string!".to_owned();
            let _s3 = "allocating a string!".to_owned();
            let _s4 = "allocating a string!".to_owned();
            let jh2 = std::thread::Builder::new()
                .name("Boo thread 1".to_string())
                .spawn(|| {
                    let _s2 = "allocating a string!".to_owned();
                    let _s3 = "allocating a string!".to_owned();
                    let _s4 = "allocating a string!".to_owned();
                    std::thread::sleep(Duration::from_millis(200));
                })
                .unwrap();
            std::thread::sleep(Duration::from_millis(200));
            jh2.join().unwrap();
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    print_allocations();
    jh1.join().unwrap();
    print_allocations();
    deinit_allocator();
}
