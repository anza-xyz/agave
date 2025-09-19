use {
    anyhow::{anyhow, Context, Result},
    clap::{Args, ValueEnum},
    log::{debug, info},
    std::{fmt, fs, process::Command},
    toml_edit::{value, DocumentMut},
};

#[derive(Args)]
pub struct BumpArgs {
    #[arg(value_enum)]
    pub level: BumpLevel,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum BumpLevel {
    Major,
    Minor,
    Patch,
}

impl fmt::Display for BumpLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BumpLevel::Major => "major",
            BumpLevel::Minor => "minor",
            BumpLevel::Patch => "patch",
        };
        write!(f, "{s}")
    }
}

pub fn run(args: BumpArgs) -> Result<()> {
    // get the current version
    let current_version =
        crate::common::get_current_version().context("failed to get current version")?;

    // bump the version
    let new_version = bump_version(&args.level.to_string().to_lowercase(), &current_version);

    // get all crates
    let all_crates = crate::common::get_all_crates().context("failed to get all crates")?;

    // update all cargo.toml
    let all_cargo_tomls =
        crate::common::find_all_cargo_tomls().context("failed to find all cargo.toml files")?;
    info!("found {} cargo.toml files", all_cargo_tomls.len());
    for cargo_toml in all_cargo_tomls {
        info!("processing {}", cargo_toml.display());

        // parse the cargo.toml file into a DocumentMut
        let content = fs::read_to_string(&cargo_toml)
            .context(format!("failed to read {}", cargo_toml.display()))?;
        let mut doc = content
            .parse::<DocumentMut>()
            .context(format!("failed to parse {}", cargo_toml.display()))?;

        // check if workspace.package.version is the same as the current version
        if let Some(workspace_package_version_str) = doc
            .get("workspace")
            .and_then(|workspace| workspace.get("package"))
            .and_then(|package| package.get("version"))
            .and_then(|version| version.as_str())
        {
            if workspace_package_version_str == current_version {
                doc["workspace"]["package"]["version"] = value(&new_version);
                info!("  bumped workspace.package.version from {current_version} to {new_version}",);
            }
        }

        // check if package.version is the same as the current version
        if let Some(package_version_str) = doc
            .get("package")
            .and_then(|package| package.get("version"))
            .and_then(|version| version.as_str())
        {
            if package_version_str == current_version {
                doc["package"]["version"] = value(&new_version);
                info!("  bumped package.version from {current_version} to {current_version}",);
            }
        }

        // Update versions in [workspace.dependencies] if they match `current_version`
        if let Some(dependencies) = doc
            .get("workspace")
            .and_then(|ws| ws.get("dependencies"))
            .and_then(|deps| deps.as_table())
        {
            // Avoid borrowing `doc` while iterating
            let keys: Vec<String> = dependencies.iter().map(|(k, _)| k.to_string()).collect();

            for name in keys {
                if all_crates.contains(&name) {
                    if let Some(version) = doc["workspace"]["dependencies"]
                        .get(&name)
                        .and_then(|v| v.get("version"))
                        .and_then(|v| v.as_str())
                    {
                        if !version.contains(&current_version) {
                            continue;
                        }
                        let old_version = version.to_string();
                        let new_version = old_version.replace(&current_version, &new_version);
                        doc["workspace"]["dependencies"][&name]["version"] = value(&new_version);
                        info!(
                            "  bumped workspace.dependencies.{name}.version from {old_version} to \
                             {new_version}",
                        );
                    }
                }
            }
        }

        // write the updated document back to the file
        debug!("writing {}", cargo_toml.display());
        fs::write(&cargo_toml, doc.to_string())
            .context(format!("failed to write {}", cargo_toml.display()))?;
    }

    // update all Cargo.lock files
    let all_cargo_locks =
        crate::common::find_all_cargo_locks().context("failed to find all Cargo.lock files")?;
    info!("found {} Cargo.lock files", all_cargo_locks.len());
    for cargo_lock in all_cargo_locks {
        let dir = cargo_lock.parent().context(format!(
            "failed to get {}'s parent directory",
            cargo_lock.display()
        ))?;

        info!("running `cargo tree` in {}", dir.display());
        let output = Command::new("cargo")
            .arg("tree")
            .current_dir(dir)
            .output()
            .context(format!("failed to run `cargo tree` in {}", dir.display()))?;
        if !output.status.success() {
            return Err(anyhow!("{}", String::from_utf8_lossy(&output.stderr)));
        }
    }

    // run `git diff --unified=0 ./**/Cargo.lock`
    let git_root = crate::common::get_git_root_path().context("failed to get git root path")?;
    info!(
        "running `git diff --unified=0 ./**/Cargo.lock` in {}",
        git_root.display()
    );
    let output = Command::new("bash")
        .arg("-c")
        .arg("shopt -s globstar; git diff --unified=0 ./**/Cargo.lock")
        .current_dir(&git_root)
        .output()
        .context("failed to run `git diff --unified=0`")?;
    if !output.status.success() {
        return Err(anyhow!("{}", String::from_utf8_lossy(&output.stderr)));
    }

    info!(
        "writing diff to {}",
        git_root.join("cargo-lock-patch").display()
    );
    let diff_output = String::from_utf8_lossy(&output.stdout);
    let patch_file_path = git_root.join("cargo-lock-patch");
    fs::write(&patch_file_path, diff_output.as_bytes())
        .context(format!("failed to write to {}", patch_file_path.display()))?;

    let mut filtered_diff_output = String::with_capacity(diff_output.len());
    let mut lines = diff_output.lines().peekable();
    while let Some(line) = lines.next() {
        if !line.starts_with("@@") {
            filtered_diff_output.push_str(line);
            filtered_diff_output.push('\n');
            continue;
        }

        let mut block = vec![];
        block.push(line);
        while let Some(next_line) = lines.peek() {
            if next_line.starts_with("@@") {
                break;
            }
            let next_line = lines.next().unwrap();
            block.push(next_line);
        }

        let target_crates = ["hashbrown", "itertools"];
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();

        for block_line in &block {
            for target_crate in target_crates {
                if block_line.contains(target_crate) {
                    let counter = counts.entry(target_crate).or_default();
                    *counter = counter.saturating_add(1);
                }
            }
        }

        // Filter out lines where the crate appears exactly twice (once `-` and once `+`)
        let filtered_block: Vec<&str> = block
            .iter()
            .copied()
            .filter(|line| {
                for &name in &target_crates {
                    if line.contains(name) {
                        return counts.get(name) != Some(&2);
                    }
                }
                true
            })
            .collect();

        if filtered_block.len() == 1 {
            continue;
        }
        for line in filtered_block {
            filtered_diff_output.push_str(line);
            filtered_diff_output.push('\n');
        }
    }
    let filtered_patch_file_path = git_root.join("cargo-lock-patch-filtered");
    info!(
        "writing filtered diff to {}",
        filtered_patch_file_path.display()
    );
    fs::write(&filtered_patch_file_path, filtered_diff_output.as_bytes()).context(format!(
        "failed to write to {}",
        filtered_patch_file_path.display()
    ))?;

    let command = "shopt -s globstar; git ls-files -- **/Cargo.lock | xargs -I {} git checkout {}";
    info!("running `{}` in {}", command, git_root.display());
    let output = Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(&git_root)
        .output()
        .context(format!("failed to run `{command}`"))?;
    if !output.status.success() {
        return Err(anyhow!("{}", String::from_utf8_lossy(&output.stderr)));
    }

    let command = format!(
        "git apply --unidiff-zero {}",
        filtered_patch_file_path.display()
    );
    info!("running `{}` in {}", command, git_root.display());
    let output = Command::new("bash")
        .arg("-c")
        .arg(&command)
        .current_dir(&git_root)
        .output()
        .context(format!("failed to run `{command}`"))?;
    if !output.status.success() {
        return Err(anyhow!("{}", String::from_utf8_lossy(&output.stderr)));
    }

    Ok(())
}

pub fn bump_version(level: &str, current: &str) -> String {
    let mut parts: Vec<u32> = current.split('.').map(|s| s.parse().unwrap_or(0)).collect();

    match level {
        "major" => {
            parts[0] = parts[0].saturating_add(1);
            parts[1] = 0;
            parts[2] = 0;
        }
        "minor" => {
            parts[1] = parts[1].saturating_add(1);
            parts[2] = 0;
        }
        "patch" => {
            parts[2] = parts[2].saturating_add(1);
        }
        _ => {}
    }

    format!("{}.{}.{}", parts[0], parts[1], parts[2])
}
