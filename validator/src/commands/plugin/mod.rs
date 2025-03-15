use {
    crate::{admin_rpc_service, commands::FromClapArgMatches},
    clap::{value_t, App, AppSettings, Arg, ArgMatches, SubCommand},
    std::path::Path,
};

const COMMAND: &str = "plugin";

#[derive(Debug, PartialEq)]
pub struct PluginUnloadArgs {
    pub name: String,
}

impl FromClapArgMatches for PluginUnloadArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self, String> {
        Ok(PluginUnloadArgs {
            name: value_t!(matches, "name", String).map_err(|_| "invalid name".to_string())?,
        })
    }
}

#[derive(Debug, PartialEq)]
pub struct PluginLoadArgs {
    pub config: String,
}

impl FromClapArgMatches for PluginLoadArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self, String> {
        Ok(PluginLoadArgs {
            config: value_t!(matches, "config", String)
                .map_err(|_| "invalid config".to_string())?,
        })
    }
}

#[derive(Debug, PartialEq)]
pub struct PluginReloadArgs {
    pub name: String,
    pub config: String,
}

impl FromClapArgMatches for PluginReloadArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self, String> {
        Ok(PluginReloadArgs {
            name: value_t!(matches, "name", String).map_err(|_| "invalid name".to_string())?,
            config: value_t!(matches, "config", String)
                .map_err(|_| "invalid config".to_string())?,
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
    let name_arg = Arg::with_name("name").required(true).takes_value(true);
    let config_arg = Arg::with_name("config").required(true).takes_value(true);

    SubCommand::with_name(COMMAND)
        .about("Manage and view geyser plugins")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .setting(AppSettings::InferSubcommands)
        .subcommand(SubCommand::with_name("list").about("List all current running gesyer plugins"))
        .subcommand(
            SubCommand::with_name("unload")
                .about("Unload a particular gesyer plugin. You must specify the gesyer plugin name")
                .arg(&name_arg),
        )
        .subcommand(
            SubCommand::with_name("reload")
                .about(
                    "Reload a particular gesyer plugin. You must specify the gesyer plugin name \
                     and the new config path",
                )
                .arg(&name_arg)
                .arg(&config_arg),
        )
        .subcommand(
            SubCommand::with_name("load")
                .about(
                    "Load a new gesyer plugin. You must specify the config path. Fails if \
                     overwriting (use reload)",
                )
                .arg(&config_arg),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<(), String> {
    match matches.subcommand() {
        ("list", _) => {
            let admin_client = admin_rpc_service::connect(ledger_path);
            let plugins = admin_rpc_service::runtime()
                .block_on(async move { admin_client.await?.list_plugins().await })
                .map_err(|err| format!("list plugins request failed: {err}"))?;
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
            let PluginUnloadArgs { name } =
                PluginUnloadArgs::from_clap_arg_match(subcommand_matches)?;

            let admin_client = admin_rpc_service::connect(ledger_path);
            admin_rpc_service::runtime()
                .block_on(async { admin_client.await?.unload_plugin(name.clone()).await })
                .map_err(|err| format!("unload plugin request failed: {err:?}"))?;
            println!("Successfully unloaded plugin: {name}");
        }
        ("load", Some(subcommand_matches)) => {
            let PluginLoadArgs { config } =
                PluginLoadArgs::from_clap_arg_match(subcommand_matches)?;

            let admin_client = admin_rpc_service::connect(ledger_path);
            let name = admin_rpc_service::runtime()
                .block_on(async { admin_client.await?.load_plugin(config.clone()).await })
                .map_err(|err| format!("load plugin request failed {config}: {err:?}"))?;
            println!("Successfully loaded plugin: {name}");
        }
        ("reload", Some(subcommand_matches)) => {
            let PluginReloadArgs { name, config } =
                PluginReloadArgs::from_clap_arg_match(subcommand_matches)?;

            let admin_client = admin_rpc_service::connect(ledger_path);
            admin_rpc_service::runtime()
                .block_on(async {
                    admin_client
                        .await?
                        .reload_plugin(name.clone(), config.clone())
                        .await
                })
                .map_err(|err| format!("reload plugin request failed {name}: {err:?}"))?;
            println!("Successfully reloaded plugin: {name}");
        }
        _ => unreachable!(),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use {super::*, crate::commands::tests::verify_args_struct_by_command_is_error};

    #[test]
    fn verify_args_struct_by_command_plugin_unload_default() {
        verify_args_struct_by_command_is_error::<PluginUnloadArgs>(
            command(),
            vec![COMMAND, "unload"],
        );
    }

    #[test]
    fn verify_args_struct_by_command_plugin_unload_with_name() {
        let app = command();
        let matches = app.get_matches_from(vec![COMMAND, "unload", "testname"]);
        let subcommand_matches = matches.subcommand_matches("unload").unwrap();
        let args = PluginUnloadArgs::from_clap_arg_match(subcommand_matches).unwrap();
        assert_eq!(
            args,
            PluginUnloadArgs {
                name: "testname".to_string(),
            }
        );
    }

    #[test]
    fn verify_args_struct_by_command_plugin_load_default() {
        verify_args_struct_by_command_is_error::<PluginLoadArgs>(command(), vec![COMMAND, "load"]);
    }

    #[test]
    fn verify_args_struct_by_command_plugin_load_with_config() {
        let app = command();
        let matches = app.get_matches_from(vec![COMMAND, "load", "testconfig"]);
        let subcommand_matches = matches.subcommand_matches("load").unwrap();
        let args = PluginLoadArgs::from_clap_arg_match(subcommand_matches).unwrap();
        assert_eq!(
            args,
            PluginLoadArgs {
                config: "testconfig".to_string(),
            }
        );
    }

    #[test]
    fn verify_args_struct_by_command_plugin_reload_default() {
        verify_args_struct_by_command_is_error::<PluginReloadArgs>(
            command(),
            vec![COMMAND, "reload"],
        );
    }

    #[test]
    fn verify_args_struct_by_command_plugin_reload_with_name() {
        verify_args_struct_by_command_is_error::<PluginReloadArgs>(
            command(),
            vec![COMMAND, "reload", "testname"],
        );
    }

    #[test]
    fn verify_args_struct_by_command_plugin_reload_with_name_and_config() {
        let app = command();
        let matches = app.get_matches_from(vec![COMMAND, "reload", "testname", "testconfig"]);
        let subcommand_matches = matches.subcommand_matches("reload").unwrap();
        let args = PluginReloadArgs::from_clap_arg_match(subcommand_matches).unwrap();
        assert_eq!(
            args,
            PluginReloadArgs {
                name: "testname".to_string(),
                config: "testconfig".to_string(),
            }
        );
    }
}
