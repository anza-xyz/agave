mod fetch;
mod resolve;

use {
    anyhow::Result,
    futures_util::future::try_join_all,
    resolve::{EnvInputs, Xy, derive_channels, print_channel_info},
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
            .map(|xy| fetch::workspace_version(&client, xy)),
    )
    .await?;
    let versions: BTreeMap<Xy, Version> = heads.into_iter().zip(fetched).collect();

    let info = derive_channels(&versions, &tags, &read_env())?;
    print_channel_info(&info);

    Ok(())
}

fn pick_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}

fn read_env() -> EnvInputs {
    EnvInputs {
        branch: pick_env("CI_BASE_BRANCH").or_else(|| pick_env("CI_BRANCH")),
        channel: pick_env("CHANNEL"),
    }
}
