use {
    crate::{
        admin_rpc_service,
        commands::{FromClapArgMatches, Result},
    },
    clap::{App, Arg, ArgGroup, ArgMatches, SubCommand},
    std::{net::SocketAddr, path::Path},
};

const COMMAND: &str = "set-public-address";

#[derive(Debug, PartialEq)]
pub struct SetPublicAddressArgs {
    pub tpu_addr: Option<SocketAddr>,
    pub tpu_forwards_addr: Option<SocketAddr>,
    pub tvu_addr: Option<SocketAddr>,
}

impl FromClapArgMatches for SetPublicAddressArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let parse_arg_addr =
            |arg_name: &str,
             arg_long: &str|
             -> std::result::Result<Option<SocketAddr>, Box<dyn std::error::Error>> {
                Ok(matches
                    .value_of(arg_name)
                    .map(|host_port| {
                        solana_net_utils::parse_host_port(host_port).map_err(|err| {
                            format!(
                                "failed to parse --{arg_long} address. It must be in the \
                                 HOST:PORT format. {err}"
                            )
                        })
                    })
                    .transpose()?)
            };
        Ok(SetPublicAddressArgs {
            tpu_addr: parse_arg_addr("tpu_addr", "tpu")?,
            tpu_forwards_addr: parse_arg_addr("tpu_forwards_addr", "tpu-forwards")?,
            tvu_addr: parse_arg_addr("tvu_addr", "tvu")?,
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
        .about("Specify addresses to advertise in gossip")
        .arg(
            Arg::with_name("tpu_addr")
                .long("tpu")
                .value_name("HOST:PORT")
                .takes_value(true)
                .validator(solana_net_utils::is_host_port)
                .help("TPU address to advertise in gossip"),
        )
        .arg(
            Arg::with_name("tpu_forwards_addr")
                .long("tpu-forwards")
                .value_name("HOST:PORT")
                .takes_value(true)
                .validator(solana_net_utils::is_host_port)
                .help("TPU Forwards address to advertise in gossip"),
        )
        .arg(
            Arg::with_name("tvu_addr")
                .long("tvu")
                .value_name("HOST:PORT")
                .takes_value(true)
                .validator(solana_net_utils::is_host_port)
                .help("TVU address to advertise in gossip"),
        )
        .group(
            ArgGroup::with_name("set_public_address_details")
                .args(&["tpu_addr", "tpu_forwards_addr", "tvu_addr"])
                .required(true)
                .multiple(true),
        )
        .after_help("Note: At least one arg must be used. Using multiple is ok")
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    let set_public_address_args = SetPublicAddressArgs::from_clap_arg_match(matches)?;

    macro_rules! set_public_address {
        ($public_addr:expr, $set_public_address:ident) => {
            if let Some(public_addr) = $public_addr {
                let admin_client = admin_rpc_service::connect(ledger_path);
                admin_rpc_service::runtime().block_on(async move {
                    admin_client.await?.$set_public_address(public_addr).await
                })
            } else {
                Ok(())
            }
        };
    }
    set_public_address!(set_public_address_args.tpu_addr, set_public_tpu_address)?;
    set_public_address!(
        set_public_address_args.tpu_forwards_addr,
        set_public_tpu_forwards_address
    )?;
    set_public_address!(set_public_address_args.tvu_addr, set_public_tvu_address)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::commands::tests::{
            verify_args_struct_by_command, verify_args_struct_by_command_is_error,
        },
    };

    #[test]
    fn verify_args_struct_by_command_set_public_default() {
        verify_args_struct_by_command_is_error::<SetPublicAddressArgs>(command(), vec![COMMAND]);
    }

    #[test]
    fn verify_args_struct_by_command_set_public_address_tpu() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--tpu", "127.0.0.1:8080"],
            SetPublicAddressArgs {
                tpu_addr: Some(SocketAddr::from(([127, 0, 0, 1], 8080))),
                tpu_forwards_addr: None,
                tvu_addr: None,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_set_public_address_tpu_forwards() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--tpu-forwards", "127.0.0.1:8081"],
            SetPublicAddressArgs {
                tpu_addr: None,
                tpu_forwards_addr: Some(SocketAddr::from(([127, 0, 0, 1], 8081))),
                tvu_addr: None,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_set_public_address_tpu_and_tpu_forwards() {
        verify_args_struct_by_command(
            command(),
            vec![
                COMMAND,
                "--tpu",
                "127.0.0.1:8080",
                "--tpu-forwards",
                "127.0.0.1:8081",
                "--tvu",
                "127.0.0.1:8082",
            ],
            SetPublicAddressArgs {
                tpu_addr: Some(SocketAddr::from(([127, 0, 0, 1], 8080))),
                tpu_forwards_addr: Some(SocketAddr::from(([127, 0, 0, 1], 8081))),
                tvu_addr: Some(SocketAddr::from(([127, 0, 0, 1], 8082))),
            },
        );
    }
}
