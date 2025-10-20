use {
    anyhow::{anyhow, Context, Result},
    clap::{Args, ValueEnum},
    log::{debug, info},
    semver::Version,
    std::{fmt, fs, process::Command},
    toml_edit::{value, DocumentMut},
};

#[derive(Args)]
pub struct CommandArgs {
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

pub fn run(args: CommandArgs) -> Result<()> {
    // get the current version
    let current_version_str =
        crate::common::get_current_version().context("failed to get current version")?;
    let current_version = Version::parse(&current_version_str)?;

    // bump the version
    let new_version = bump_version(&args.level, &current_version);

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
            if workspace_package_version_str == current_version.to_string() {
                doc["workspace"]["package"]["version"] = value(new_version.to_string());
                info!("  bumped workspace.package.version from {current_version} to {new_version}",);
            }
        }

        // check if package.version is the same as the current version
        if let Some(package_version_str) = doc
            .get("package")
            .and_then(|package| package.get("version"))
            .and_then(|version| version.as_str())
        {
            if package_version_str == current_version.to_string() {
                doc["package"]["version"] = value(new_version.to_string());
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
                        if !version.contains(&current_version.to_string()) {
                            continue;
                        }
                        let old_version = version.to_string();
                        let new_version = old_version
                            .replace(&current_version.to_string(), &new_version.to_string());
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

    Ok(())
}

pub fn bump_version(level: &BumpLevel, current: &Version) -> Version {
    let mut new_version = current.clone();
    match level {
        BumpLevel::Major => {
            new_version.major = new_version.major.saturating_add(1);
            new_version.minor = 0;
            new_version.patch = 0;
        }
        BumpLevel::Minor => {
            new_version.minor = new_version.minor.saturating_add(1);
            new_version.patch = 0;
        }
        BumpLevel::Patch => {
            new_version.patch = new_version.patch.saturating_add(1);
        }
    }

    new_version
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bump_version() {
        // bump major
        {
            assert_eq!(
                bump_version(&BumpLevel::Major, &Version::parse("1.0.0").unwrap()),
                Version::parse("2.0.0").unwrap()
            );

            assert_eq!(
                bump_version(&BumpLevel::Major, &Version::parse("1.1.0").unwrap()),
                Version::parse("2.0.0").unwrap()
            );

            assert_eq!(
                bump_version(&BumpLevel::Major, &Version::parse("1.1.1").unwrap()),
                Version::parse("2.0.0").unwrap()
            );
        }

        // bump minor
        {
            assert_eq!(
                bump_version(&BumpLevel::Minor, &Version::parse("1.0.0").unwrap()),
                Version::parse("1.1.0").unwrap()
            );

            assert_eq!(
                bump_version(&BumpLevel::Minor, &Version::parse("1.2.1").unwrap()),
                Version::parse("1.3.0").unwrap()
            );
        }

        // bump patch
        {
            assert_eq!(
                bump_version(&BumpLevel::Patch, &Version::parse("1.0.0").unwrap()),
                Version::parse("1.0.1").unwrap()
            );
        }
    }
}
