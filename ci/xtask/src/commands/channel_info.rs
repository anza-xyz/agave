mod fetch;
mod resolve;

use {
    anyhow::Result,
    clap::Args,
    futures_util::future::try_join_all,
    resolve::{BranchVersion, derive_channels, print_channel_info, print_channel_info_json},
    semver::Version,
    std::{collections::BTreeMap, env},
};

#[derive(Args)]
pub struct CommandArgs {
    /// Emit channel info as JSON instead of eval-able shell assignments
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: CommandArgs) -> Result<()> {
    let mut heads = fetch::release_heads()?;
    let tags = fetch::release_tags()?;

    heads.sort();
    heads.reverse();
    heads.truncate(3);

    let client = reqwest::Client::new();
    let fetched = try_join_all(
        heads
            .iter()
            .copied()
            .map(|bv| fetch::workspace_version(&client, bv)),
    )
    .await?;
    let versions: BTreeMap<BranchVersion, Version> = heads.into_iter().zip(fetched).collect();

    let pins = fetch::channel_pins(&client).await?;
    let branch = pick_env("CI_BASE_BRANCH").or_else(|| pick_env("CI_BRANCH"));
    let channel = pick_env("CHANNEL");
    let info = derive_channels(
        &versions,
        &tags,
        branch.as_deref(),
        channel.as_deref(),
        &pins,
    )?;

    if args.json {
        print_channel_info_json(&info)?;
    } else {
        print_channel_info(&info);
    }

    Ok(())
}

fn pick_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}
