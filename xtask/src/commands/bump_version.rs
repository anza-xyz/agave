use {
    clap::{Args, ValueEnum},
    log::{debug, info},
    std::{fmt, fs},
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

pub fn run(args: BumpArgs) -> Result<(), Box<dyn std::error::Error>> {
    let current_version = crate::common::get_current_version()
        .map_err(|e| format!("Failed to get current version: error={}", e))?;

    let new_version = bump_version(&args.level.to_string().to_lowercase(), &current_version);

    let all_crates = crate::common::get_all_crates();

    // update all cargo.toml files in the workspace
    let all_cargo_tomls = crate::common::find_all_cargo_tomls();
    info!("found {} cargo.toml files", all_cargo_tomls.len());
    for cargo_toml in all_cargo_tomls {
        info!("processing {}", cargo_toml.display());

        // parse the cargo.toml file into a DocumentMut
        let content = fs::read_to_string(&cargo_toml)?;
        let mut doc = content.parse::<DocumentMut>().map_err(|e| {
            format!(
                "Failed to read Cargo.toml: path={}, error={}",
                cargo_toml.display(),
                e
            )
        })?;

        // check if workspace.package.version is the same as the current version
        if let Some(workspace_package_version_str) = doc
            .get("workspace")
            .and_then(|workspace| workspace.get("package"))
            .and_then(|package| package.get("version"))
            .and_then(|version| version.as_str())
        {
            if workspace_package_version_str == current_version {
                doc["workspace"]["package"]["version"] = value(&new_version);
                info!(
                    "bumped workspace.package.version from {} to {}",
                    current_version, new_version
                );
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
                info!(
                    "bumped package.version from {} to {}",
                    current_version, new_version
                );
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
                            "bumped workspace.dependencies.{}.version from {} to {}",
                            name, old_version, new_version
                        );
                    }
                }
            }
        }

        // write the updated document back to the file
        debug!("writing {}", cargo_toml.display());
        fs::write(&cargo_toml, doc.to_string()).map_err(|e| {
            format!(
                "Failed to write Cargo.toml: path={}, error={}",
                cargo_toml.display(),
                e
            )
        })?;
    }

    Ok(())
}

pub fn bump_version(level: &str, current: &str) -> String {
    let mut parts: Vec<u32> = current.split('.').map(|s| s.parse().unwrap_or(0)).collect();

    match level {
        "major" => {
            parts[0] += 1;
            parts[1] = 0;
            parts[2] = 0;
        }
        "minor" => {
            parts[1] += 1;
            parts[2] = 0;
        }
        "patch" => {
            parts[2] += 1;
        }
        _ => {}
    }

    format!("{}.{}.{}", parts[0], parts[1], parts[2])
}
