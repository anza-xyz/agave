use std::sync::LazyLock;

pub static CONFIG_FILE: LazyLock<Option<String>> = LazyLock::new(|| {
    dirs_next::home_dir().map(|mut path| {
        path.extend([".config", "solana", "install", "config.yml"]);
        path.to_str().unwrap().to_string()
    })
});

pub static DATA_DIR: LazyLock<Option<String>> = LazyLock::new(|| {
    dirs_next::home_dir().map(|mut path| {
        path.extend([".local", "share", "solana", "install"]);
        path.to_str().unwrap().to_string()
    })
});
