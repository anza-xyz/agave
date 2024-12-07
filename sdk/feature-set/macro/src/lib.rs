extern crate proc_macro;

use {
    proc_macro::TokenStream,
    std::sync::atomic::{AtomicUsize, Ordering},
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

#[proc_macro]
pub fn unique_index(_input: TokenStream) -> TokenStream {
    // Increment and get the current value of the counter
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    // Return the unique index as a literal
    id.to_string().parse().unwrap()
}

#[proc_macro]
pub fn get_count(_input: TokenStream) -> TokenStream {
    // Get the current value of the counter
    let id = COUNTER.load(Ordering::SeqCst);
    // Return the current value of the counter as a literal
    id.to_string().parse().unwrap()
}
