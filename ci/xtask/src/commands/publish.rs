use {
    anyhow::{anyhow, Result},
    cargo_metadata::{MetadataCommand, PackageId},
    clap::{Args, Subcommand},
    serde::Serialize,
    std::{
        collections::{HashMap, HashSet},
        path::{Path, PathBuf},
    },
};

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum OutputFormat {
    Json,
    Tree,
}

#[derive(Subcommand)]
pub enum PublishSubcommand {
    #[command(about = "Print the publish order")]
    Order {
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
}

#[derive(Args)]
pub struct CommandArgs {
    #[arg(long, default_value = "Cargo.toml")]
    pub manifest_path: String,

    #[command(subcommand)]
    pub subcommand: PublishSubcommand,
}

pub fn run(args: CommandArgs) -> Result<()> {
    match args.subcommand {
        PublishSubcommand::Order { format } => match format {
            OutputFormat::Json => publish_order_json(&args.manifest_path)?,
            OutputFormat::Tree => publish_order_tree(&args.manifest_path)?,
        },
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageInfo {
    pub name: String,
    pub path: PathBuf,
    pub dependencies: HashSet<PackageId>,
}

#[derive(Debug)]
pub struct PublishOrderData {
    pub levels: Vec<Vec<PackageId>>,
    pub id_to_level: HashMap<PackageId, usize>,
    pub id_to_package_info: HashMap<PackageId, PackageInfo>,
}

pub fn compute_publish_order_data(manifest_path: &str) -> Result<PublishOrderData> {
    let mut cmd = MetadataCommand::new();
    cmd.manifest_path(manifest_path);
    let metadata = cmd.exec()?;

    let workspace_member_ids: HashSet<&PackageId> = metadata.workspace_members.iter().collect();

    let mut id_to_package_info: HashMap<PackageId, PackageInfo> = HashMap::new();
    for pkg in metadata.packages.iter() {
        // skip packages that are not part of the workspace
        if !workspace_member_ids.contains(&pkg.id) {
            continue;
        }

        // skip packages that no need to be published
        if let Some(registries) = &pkg.publish {
            if registries.is_empty() {
                continue;
            }
        }

        let path = Path::new(&pkg.manifest_path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        id_to_package_info.insert(
            pkg.id.clone(),
            PackageInfo {
                name: pkg.name.clone().to_string(),
                path,
                dependencies: HashSet::new(),
            },
        );
    }

    // build dependency relationships
    if let Some(resolve) = &metadata.resolve {
        for node in &resolve.nodes {
            // only process packages that are in our workspace
            if let Some(mut package_info) = id_to_package_info.get(&node.id).cloned() {
                for dep in node.deps.iter() {
                    // skip self dependencies
                    if dep.pkg == node.id {
                        continue;
                    }
                    if let Some(_) = id_to_package_info.get(&dep.pkg) {
                        package_info.dependencies.insert(dep.pkg.clone());
                    }
                }
                id_to_package_info.insert(node.id.clone(), package_info);
            }
        }
    }

    let mut levels: Vec<Vec<PackageId>> = Vec::new();
    let mut processed: HashSet<PackageId> = HashSet::new();
    let mut id_to_level: HashMap<PackageId, usize> = HashMap::new();

    loop {
        let mut current_level = vec![];
        // find all packages that have all their dependencies processed
        for (package_id, package_info) in id_to_package_info.iter() {
            if processed.contains(package_id) {
                continue;
            }
            if package_info
                .dependencies
                .iter()
                .all(|dep| processed.contains(dep))
            {
                current_level.push(package_id.clone());
            }
        }

        if current_level.is_empty() {
            break;
        }
        current_level.sort();

        // add the current level to the levels vector
        for package_id in current_level.iter().cloned() {
            id_to_level.insert(package_id, levels.len());
        }

        levels.push(current_level.iter().cloned().collect());

        // mark the packages in the current level as processed
        for package_id in current_level.iter().cloned() {
            processed.insert(package_id);
        }
    }

    // check for unprocessed packages
    let mut unprocessed_packages = vec![];
    for package_id in id_to_package_info.keys() {
        if !processed.contains(package_id) {
            let package_info = id_to_package_info.get(package_id).unwrap();
            unprocessed_packages.push(package_info.name.clone());
        }
    }
    if !unprocessed_packages.is_empty() {
        return Err(anyhow!(
            "Unprocessed packages found: {:?}",
            unprocessed_packages
        ));
    }

    Ok(PublishOrderData {
        levels,
        id_to_level,
        id_to_package_info,
    })
}

pub fn publish_order_json(manifest_path: &str) -> Result<()> {
    let publish_order_data = compute_publish_order_data(manifest_path)?;

    let mut output = vec![];
    for level in publish_order_data.levels.iter() {
        let mut level_output = vec![];
        for package_id in level.iter() {
            let package_info = publish_order_data
                .id_to_package_info
                .get(package_id)
                .unwrap();
            level_output.push(package_info.to_owned());
        }
        output.push(level_output);
    }

    let json = serde_json::to_string(&output)?;
    println!("{json}");

    Ok(())
}

pub fn publish_order_tree(manifest_path: &str) -> Result<()> {
    let publish_order_data = compute_publish_order_data(manifest_path)?;

    let total_packages = publish_order_data
        .levels
        .iter()
        .map(|level| level.len())
        .sum::<usize>();
    let total_levels = publish_order_data.levels.len();

    println!("📦 Total packages: {total_packages}");
    println!("🌳 Total levels: {total_levels}");
    println!();

    for (level, package_ids) in publish_order_data.levels.iter().enumerate() {
        println!(
            "L{}: ({} package(s))",
            level.saturating_add(1),
            package_ids.len()
        );

        for package_id in package_ids {
            let package_info = publish_order_data
                .id_to_package_info
                .get(package_id)
                .unwrap();
            let package_name = &package_info.name;
            let dependencies = &package_info.dependencies;

            println!("  {}", package_name);

            if !dependencies.is_empty() {
                // build a map of level -> dependencies
                let mut dependencies_by_level: HashMap<usize, Vec<String>> = HashMap::new();
                for dependency_package_id in dependencies.iter() {
                    if let Some(&dependency_level) =
                        publish_order_data.id_to_level.get(dependency_package_id)
                    {
                        let dependency_package_name = &publish_order_data
                            .id_to_package_info
                            .get(dependency_package_id)
                            .unwrap()
                            .name;
                        dependencies_by_level
                            .entry(dependency_level)
                            .or_insert_with(Vec::new)
                            .push(dependency_package_name.clone());
                    }
                }

                // sort levels
                let mut sorted_levels: Vec<_> = dependencies_by_level.keys().copied().collect();
                sorted_levels.sort();

                for dependency_level in sorted_levels {
                    println!(
                        "    L{}: {:?}",
                        dependency_level.saturating_add(1),
                        dependencies_by_level[&dependency_level]
                    );
                }
            }
        }
        println!();
    }

    Ok(())
}
