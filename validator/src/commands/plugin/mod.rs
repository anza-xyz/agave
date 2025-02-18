use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{value_t, App, AppSettings, Arg, ArgMatches, SubCommand},
    std::{path::Path, process::exit},
};

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name("plugin")
        .about("Manage and view geyser plugins")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .setting(AppSettings::InferSubcommands)
        .subcommand(SubCommand::with_name("list").about("List all current running gesyer plugins"))
        .subcommand(
            SubCommand::with_name("unload")
                .about("Unload a particular gesyer plugin. You must specify the gesyer plugin name")
                .arg(Arg::with_name("name").required(true).takes_value(true)),
        )
        .subcommand(
            SubCommand::with_name("reload")
                .about(
                    "Reload a particular gesyer plugin. You must specify the gesyer plugin name \
                     and the new config path",
                )
                .arg(Arg::with_name("name").required(true).takes_value(true))
                .arg(Arg::with_name("config").required(true).takes_value(true)),
        )
        .subcommand(
            SubCommand::with_name("load")
                .about(
                    "Load a new gesyer plugin. You must specify the config path. Fails if \
                     overwriting (use reload)",
                )
                .arg(Arg::with_name("config").required(true).takes_value(true)),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    match matches.subcommand() {
        ("list", _) => {
            let admin_client = admin_rpc_service::connect(ledger_path);
            let plugins = admin_rpc_service::runtime()
                .block_on(async move { admin_client.await?.list_plugins().await })
                .unwrap_or_else(|err| {
                    println!("Failed to list plugins: {err}");
                    exit(1);
                });
            if !plugins.is_empty() {
                println!("Currently the following plugins are loaded:");
                for (plugin, i) in plugins.into_iter().zip(1..) {
                    println!("  {i}) {plugin}");
                }
            } else {
                println!("There are currently no plugins loaded");
            }
        }
        ("unload", Some(subcommand_matches)) => {
            if let Ok(name) = value_t!(subcommand_matches, "name", String) {
                let admin_client = admin_rpc_service::connect(ledger_path);
                admin_rpc_service::runtime()
                    .block_on(async { admin_client.await?.unload_plugin(name.clone()).await })
                    .unwrap_or_else(|err| {
                        println!("Failed to unload plugin {name}: {err:?}");
                        exit(1);
                    });
                println!("Successfully unloaded plugin: {name}");
            }
        }
        ("load", Some(subcommand_matches)) => {
            if let Ok(config) = value_t!(subcommand_matches, "config", String) {
                let admin_client = admin_rpc_service::connect(ledger_path);
                let name = admin_rpc_service::runtime()
                    .block_on(async { admin_client.await?.load_plugin(config.clone()).await })
                    .unwrap_or_else(|err| {
                        println!("Failed to load plugin {config}: {err:?}");
                        exit(1);
                    });
                println!("Successfully loaded plugin: {name}");
            }
        }
        ("reload", Some(subcommand_matches)) => {
            if let Ok(name) = value_t!(subcommand_matches, "name", String) {
                if let Ok(config) = value_t!(subcommand_matches, "config", String) {
                    let admin_client = admin_rpc_service::connect(ledger_path);
                    admin_rpc_service::runtime()
                        .block_on(async {
                            admin_client
                                .await?
                                .reload_plugin(name.clone(), config.clone())
                                .await
                        })
                        .unwrap_or_else(|err| {
                            println!("Failed to reload plugin {name}: {err:?}");
                            exit(1);
                        });
                    println!("Successfully reloaded plugin: {name}");
                }
            }
        }
        _ => unreachable!(),
    }
}
