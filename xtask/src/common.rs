use {
    anyhow::{anyhow, Result},
    std::{fs, path::PathBuf, process::Command},
    toml_edit::ImDocument,
    walkdir::WalkDir,
};

pub fn get_git_root_path() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| anyhow!("failed to get git root path, error: {e}"))?;
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(root))
}

pub fn find_files_by_name(filename: &str) -> Result<Vec<PathBuf>> {
    let git_root = get_git_root_path()?;
    let mut results = vec![];

    for entry in WalkDir::new(git_root)
        .into_iter()
        .filter_entry(|entry| !entry.path().components().any(|c| c.as_os_str() == "target"))
        .filter_map(Result::ok)
        .filter(|e| e.file_name() == filename)
    {
        results.push(entry.path().to_path_buf());
    }

    Ok(results)
}

pub fn find_all_cargo_tomls() -> Result<Vec<PathBuf>> {
    find_files_by_name("Cargo.toml")
}

pub fn find_all_cargo_locks() -> Result<Vec<PathBuf>> {
    find_files_by_name("Cargo.lock")
}

pub fn get_all_crates() -> Result<Vec<String>> {
    let cargo_tomls = find_all_cargo_tomls()?;
    let mut crates = vec![];
    for cargo_toml in cargo_tomls {
        let content = fs::read_to_string(cargo_toml)?;
        let doc = content.parse::<ImDocument<String>>()?;
        let Some(name) = doc
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(|name| name.as_str())
        else {
            continue;
        };
        crates.push(name.to_string());
    }
    Ok(crates)
}

pub fn get_current_version() -> Result<String> {
    let git_root = get_git_root_path()?;
    let cargo_toml = git_root.join("Cargo.toml");
    let content = fs::read_to_string(cargo_toml)?;
    let doc = content.parse::<ImDocument<String>>()?;
    let Some(version) = doc
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .and_then(|package| package.get("version"))
        .and_then(|version| version.as_str())
    else {
        return Err(anyhow!("failed to get version from Cargo.toml"));
    };
    Ok(version.to_string())
}

#[cfg(test)]
mod tests {
    use {super::*, pretty_assertions::assert_eq};

    #[test]
    fn test_get_git_root_path() {
        // create a temporary directory
        let temp_dir = tempfile::tempdir().unwrap();

        // cd into the temporary directory and do git init
        std::env::set_current_dir(temp_dir.path()).unwrap();
        Command::new("git").args(["init"]).output().unwrap();

        // call get_git_root_path should get the temporary directory
        let root_path = get_git_root_path().unwrap();

        // canonicalize the paths since macos may return a symlink
        let canonicalized_root_path = fs::canonicalize(root_path).unwrap();
        let canonicalized_temp_dir_path = fs::canonicalize(temp_dir.path()).unwrap();

        assert_eq!(canonicalized_root_path, canonicalized_temp_dir_path);
    }
}
