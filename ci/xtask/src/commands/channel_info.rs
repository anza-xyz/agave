mod fetch;
mod resolve;

use {
    anyhow::Result,
    futures_util::future::try_join_all,
    resolve::{BranchVersion, derive_channels, print_channel_info},
    semver::Version,
    std::{collections::BTreeMap, env},
};

pub async fn run() -> Result<()> {
    let client = fetch::github_client()?;

    let mut heads = fetch::release_heads(&client).await?;
    let tags = fetch::release_tags(&client).await?;

    heads.sort();
    heads.reverse();
    heads.truncate(3);

    let fetched = try_join_all(
        heads
            .iter()
            .copied()
            .map(|bv| fetch::workspace_version(&client, bv)),
    )
    .await?;
    let versions: BTreeMap<BranchVersion, Version> = heads.into_iter().zip(fetched).collect();

    let branch = pick_env("CI_BASE_BRANCH").or_else(|| pick_env("CI_BRANCH"));
    let channel = pick_env("CHANNEL");
    let info = derive_channels(&versions, &tags, branch.as_deref(), channel.as_deref())?;
    print_channel_info(&info);

    Ok(())
}

fn pick_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}
