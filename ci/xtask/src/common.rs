use {
    anyhow::{anyhow, Result},
    std::{fs, path::PathBuf, process::Command},
    toml_edit::Document,
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
        .filter_entry(|entry| {
            !entry
                .path()
                .components()
                .any(|c| c.as_os_str() == "target" || c.as_os_str() == ".git")
        })
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
        let doc = content.parse::<Document<String>>()?;
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
    let doc = content.parse::<Document<String>>()?;
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
    use {super::*, pretty_assertions::assert_eq, serial_test::serial, std::collections::HashSet};

    #[test]
    #[serial] // std::env::set_current_dir will affect other tests
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

    #[test]
    #[serial] // std::env::set_current_dir will affect other tests
    fn test_workspace_functions() {
        // $TEMPDIR
        // |-- .git
        // |-- Cargo.toml
        // |-- Cargo.lock
        // |-- foo
        // |---- Cargo.toml
        // |---- Cargo.lock
        // |-- bar
        // |---- Cargo.toml
        // |---- Cargo.lock
        let root_dir = tempfile::tempdir().unwrap();
        let root_dir_path = root_dir.path();
        std::env::set_current_dir(root_dir_path).unwrap();
        Command::new("git").args(["init"]).output().unwrap();

        // create the files
        fs::write(
            root_dir_path.join("Cargo.toml"),
            r#"
[workspace.package]
version = "3.1.0"
authors = ["Anza Maintainers <maintainers@anza.xyz>"]
description = "Blockchain, Rebuilt for Scale"
repository = "https://github.com/anza-xyz/agave"
homepage = "https://anza.xyz/"
license = "Apache-2.0"
edition = "2021"

[members]
foo = { path = "foo" }
bar = { path = "bar" }
"#,
        )
        .unwrap();
        fs::write(root_dir_path.join("Cargo.lock"), "").unwrap();
        fs::create_dir_all(root_dir_path.join("foo")).unwrap();
        fs::write(
            root_dir_path.join("foo/Cargo.toml"),
            r#"
[package]
name = "foo"
documentation = "https://docs.rs/foo"
version = { workspace = true }
authors = { workspace = true }
description = { workspace = true }
repository = { workspace = true }
homepage = { workspace = true }
license = { workspace = true }
edition = { workspace = true }
"#,
        )
        .unwrap();
        fs::write(root_dir_path.join("foo/Cargo.lock"), "").unwrap();

        fs::create_dir_all(root_dir_path.join("bar")).unwrap();
        fs::write(
            root_dir_path.join("bar/Cargo.toml"),
            r#"
[package]
name = "bar"
documentation = "https://docs.rs/bar"
version = { workspace = true }
authors = { workspace = true }
description = { workspace = true }
repository = { workspace = true }
homepage = { workspace = true }
license = { workspace = true }
edition = { workspace = true }
"#,
        )
        .unwrap();
        fs::write(root_dir_path.join("bar/Cargo.lock"), "").unwrap();

        // test find_files_by_name for Cargo.toml
        {
            let files = find_all_cargo_tomls().unwrap();
            assert_eq!(files.len(), 3);

            let expected_files: HashSet<_> = [
                fs::canonicalize(root_dir_path.join("Cargo.toml")).unwrap(),
                fs::canonicalize(root_dir_path.join("foo/Cargo.toml")).unwrap(),
                fs::canonicalize(root_dir_path.join("bar/Cargo.toml")).unwrap(),
            ]
            .iter()
            .cloned()
            .collect();

            let actual_files: HashSet<_> = files.iter().cloned().collect();

            assert_eq!(expected_files, actual_files);
        }

        // test find_files_by_name for Cargo.lock
        {
            let files = find_all_cargo_locks().unwrap();
            assert_eq!(files.len(), 3);

            let expected_files: HashSet<_> = [
                fs::canonicalize(root_dir_path.join("Cargo.lock")).unwrap(),
                fs::canonicalize(root_dir_path.join("foo/Cargo.lock")).unwrap(),
                fs::canonicalize(root_dir_path.join("bar/Cargo.lock")).unwrap(),
            ]
            .iter()
            .cloned()
            .collect();

            let actual_files: HashSet<_> = files.iter().cloned().collect();

            assert_eq!(expected_files, actual_files);
        }

        // test get_all_crates
        {
            let crates = get_all_crates().unwrap();
            assert_eq!(crates.len(), 2);
            let expected_crates: HashSet<String> =
                ["foo", "bar"].iter().map(|s| s.to_string()).collect();
            let actual_crates: HashSet<String> = crates.iter().map(|s| s.to_string()).collect();
            assert_eq!(expected_crates, actual_crates);
        }

        // test get_current_version
        {
            let version = get_current_version().unwrap();
            assert_eq!(version, "3.1.0");
        }
    }
}
