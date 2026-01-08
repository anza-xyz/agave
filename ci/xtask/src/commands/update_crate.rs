use {
    crate::common,
    anyhow::Result,
    clap::Args,
    log::{debug, info},
    std::{fs, path::PathBuf},
    toml_edit::{value, DocumentMut, Item},
};

#[derive(Args)]
pub struct CommandArgs {
    #[arg(long, default_value = ".")]
    pub root_path: PathBuf,
    #[arg(long, short, required = true)]
    pub package: String,
    #[arg(long, required = true)]
    pub from: String,
    #[arg(long, required = true)]
    pub to: String,
    #[arg(long, default_value = "[]", value_delimiter = ',')]
    pub exclude_paths: Vec<PathBuf>,
}

pub fn run(args: CommandArgs) -> Result<()> {
    let all_cargo_tomls = common::recursive_find_files(&args.root_path, "Cargo.toml", |_| true)?;

    'MAIN_LOOP: for cargo_toml in all_cargo_tomls {
        info!("[{}]", cargo_toml.display());

        for exclude_path in &args.exclude_paths {
            if cargo_toml
                .to_string_lossy()
                .contains(exclude_path.to_string_lossy().as_ref())
            {
                info!("  ⏩ skipped (exclude path)");
                continue 'MAIN_LOOP;
            }
        }

        let content = fs::read_to_string(&cargo_toml)?;
        let mut doc = content.parse::<DocumentMut>()?;
        let mut need_to_write = false;

        // update workspace.dependencies
        if let Some(workspace_deps) = doc
            .get_mut("workspace")
            .and_then(|ws| ws.get_mut("dependencies"))
            .and_then(|deps| deps.get_mut(&args.package))
        {
            if update_dependency_spec(workspace_deps, &args.from, &args.to) {
                need_to_write = true;
                info!("  ✅ updated workspace.dependencies");
            }
        }

        // update dependencies
        if let Some(deps) = doc
            .get_mut("dependencies")
            .and_then(|deps| deps.get_mut(&args.package))
        {
            if update_dependency_spec(deps, &args.from, &args.to) {
                need_to_write = true;
                info!("  ✅ updated dependencies");
            }
        }

        if need_to_write {
            fs::write(&cargo_toml, doc.to_string())?;
        } else {
            info!("  ⏩ skipped (no changes)");
        }
    }
    Ok(())
}

fn update_dependency_spec(dep_spec: &mut Item, from: &str, to: &str) -> bool {
    debug!("dep_spec: {dep_spec:?}");
    if let Some(current_version) = dep_spec.as_str() {
        if current_version == from || current_version == format!("={from}") {
            let new_version = current_version.replace(from, to);
            *dep_spec = value(&new_version);
            return true;
        }
    } else if let Some(current_version) = dep_spec
        .as_inline_table()
        .and_then(|table| table.get("version").and_then(|version| version.as_str()))
    {
        if current_version == from || current_version == format!("={from}") {
            let new_version = current_version.replace(from, to);
            dep_spec["version"] = value(&new_version);
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use {super::*, toml_edit::Table};

    #[test]
    fn test_update_dependency_spec_string() {
        // should update if the version matches
        let mut dep_spec = value("1.2.3");
        assert!(update_dependency_spec(&mut dep_spec, "1.2.3", "1.2.4"));
        assert_eq!(dep_spec.as_str(), Some("1.2.4"));

        // should not update if the version does not match
        let mut dep_spec = value("1.2.3");
        assert!(!update_dependency_spec(&mut dep_spec, "1.2.4", "1.2.5"));
        assert_eq!(dep_spec.as_str(), Some("1.2.3"));

        // should still update if the version is prefixed
        let mut dep_spec = value("=1.2.3");
        assert!(update_dependency_spec(&mut dep_spec, "1.2.3", "1.2.4"));
        assert_eq!(dep_spec.as_str(), Some("=1.2.4"));
    }

    #[test]
    fn test_update_dependency_spec_table() {
        // should update if the version matches
        let mut table = Table::default();
        table["version"] = value("1.2.3");
        let mut dep_spec: Item = value(table.into_inline_table());
        assert!(update_dependency_spec(&mut dep_spec, "1.2.3", "1.2.4"));
        assert_eq!(dep_spec["version"].as_str(), Some("1.2.4"));

        // should not update if the version does not match
        let mut table = Table::default();
        table["version"] = value("1.2.3");
        let mut dep_spec = table.into();
        assert!(!update_dependency_spec(&mut dep_spec, "1.2.4", "1.2.5"));
        assert_eq!(dep_spec["version"].as_str(), Some("1.2.3"));

        // should still update if the version is prefixed
        let mut table = Table::default();
        table["version"] = value("=1.2.3".to_string());
        let mut dep_spec: Item = value(table.into_inline_table());
        assert!(update_dependency_spec(&mut dep_spec, "1.2.3", "1.2.4"));
        assert_eq!(dep_spec["version"].as_str(), Some("=1.2.4"));
    }
}
