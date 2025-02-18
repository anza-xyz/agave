pub fn normalize_to_url_if_moniker<T: AsRef<str>>(url_or_moniker: T) -> String {
    match url_or_moniker.as_ref() {
        "m" | "mainnet-beta" => "https://api.mainnet-beta.solana.com",
        "t" | "testnet" => "https://api.testnet.solana.com",
        "d" | "devnet" => "https://api.devnet.solana.com",
        "l" | "localhost" => "http://localhost:8899",
        url => url,
    }
    .to_string()
}
