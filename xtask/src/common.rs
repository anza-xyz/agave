use {
    std::{fs, path::PathBuf, process::Command},
    toml_edit::ImDocument,
    walkdir::WalkDir,
};

pub fn get_git_root_path() -> Option<PathBuf> {
    let output = Command::new("git")
        .args(&["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(PathBuf::from(root))
}

pub fn find_all_cargo_tomls() -> Vec<PathBuf> {
    let git_root = get_git_root_path().unwrap();
    let mut results = vec![];

    let walker = WalkDir::new(git_root);

    for entry_result in walker {
        if let Ok(entry) = entry_result {
            let path = entry.path();

            // Skip unwanted directories like target/ and .git/
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                if name == "target" || name.starts_with('.') {
                    continue;
                }
            }

            // Check for Cargo.toml
            if path.is_file() && path.file_name().map_or(false, |f| f == "Cargo.toml") {
                results.push(path.to_path_buf());
            }
        }
    }

    results
}

pub fn get_all_crates() -> Vec<String> {
    let cargo_tomls = find_all_cargo_tomls();
    let mut crates = vec![];
    for cargo_toml in cargo_tomls {
        let content = fs::read_to_string(cargo_toml).unwrap();
        let doc = content.parse::<ImDocument<String>>().unwrap();
        let Some(name) = doc
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(|name| name.as_str())
        else {
            continue;
        };
        crates.push(name.to_string());
    }
    crates
}

pub fn get_current_version() -> Result<String, Box<dyn std::error::Error>> {
    let git_root = get_git_root_path().ok_or("Failed to get git root path")?;
    let cargo_toml = git_root.join("Cargo.toml");
    let content = fs::read_to_string(cargo_toml)?;
    let doc = content.parse::<ImDocument<String>>()?;
    let Some(version) = doc
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .and_then(|package| package.get("version"))
        .and_then(|version| version.as_str())
    else {
        return Err("Failed to get version from Cargo.toml".into());
    };
    Ok(version.to_string())
}
