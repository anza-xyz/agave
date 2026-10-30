use {
    super::channel_info::{self, ChannelInfo},
    crate::release_bucket::{DOWNLOAD_ROOT, ReleaseBucket, install_script},
    anyhow::{Context, Result, bail},
    clap::Args,
    log::{info, warn},
};

#[derive(Args)]
pub struct CommandArgs {
    /// Release target every channel must have, e.g. x86_64-unknown-linux-gnu
    #[arg(long = "target", required = true)]
    pub targets: Vec<String>,

    /// Log the copies instead of performing them
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: CommandArgs) -> Result<()> {
    let bucket = ReleaseBucket::new(args.dry_run)?;
    let info = channel_info::resolve().await?;
    let client = reqwest::Client::new();
    let mut incomplete = Vec::new();

    for (channel, tag) in channel_tags(&info) {
        let mut missing = Vec::new();
        for target in &args.targets {
            if !all_published(&client, tag, &target_files(target)).await? {
                missing.push(target.as_str());
            }
        }

        // Move all targets or none, so a channel never mixes releases.
        if !missing.is_empty() {
            warn!(
                "{tag} is missing {}, leaving {channel} as is",
                missing.join(", ")
            );
            incomplete.push(tag);
            continue;
        }

        info!("syncing {channel} channel from {tag}");
        for target in &args.targets {
            for file in target_files(target) {
                bucket
                    .copy(&format!("{tag}/{file}"), &format!("{channel}/{file}"))
                    .await?;
            }
        }

        bucket
            .upload(install_script(channel)?, &format!("{channel}/install"))
            .await?;
    }

    if !incomplete.is_empty() {
        bail!("incomplete releases: {}", incomplete.join(", "));
    }

    Ok(())
}

fn channel_tags(info: &ChannelInfo) -> Vec<(&'static str, &str)> {
    [
        ("stable", info.stable_channel_latest_tag.as_str()),
        ("beta", info.beta_channel_latest_tag.as_str()),
    ]
    .into_iter()
    .filter(|(_, tag)| !tag.is_empty())
    .collect()
}

fn target_files(target: &str) -> [String; 3] {
    [
        format!("solana-release-{target}.tar.bz2"),
        format!("agave-install-init-{target}"),
        format!("solana-release-{target}.yml"),
    ]
}

async fn all_published(client: &reqwest::Client, tag: &str, files: &[String]) -> Result<bool> {
    for file in files {
        let url = format!("{DOWNLOAD_ROOT}/{tag}/{file}");
        let response = client
            .head(&url)
            .send()
            .await
            .with_context(|| format!("HEAD {url}"))?;
        if !response.status().is_success() {
            return Ok(false);
        }
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(stable_tag: &str, beta_tag: &str) -> ChannelInfo {
        ChannelInfo {
            edge_channel: "master".into(),
            beta_channel: "v4.4".into(),
            beta_channel_latest_tag: beta_tag.into(),
            stable_channel: "v4.3".into(),
            stable_channel_latest_tag: stable_tag.into(),
            channel: String::new(),
            channel_latest_tag: String::new(),
        }
    }

    #[test]
    fn channel_tags_maps_each_channel_to_its_latest_tag() {
        let info = info("v4.3.2", "v4.4.0-rc.1");
        assert_eq!(
            channel_tags(&info),
            vec![("stable", "v4.3.2"), ("beta", "v4.4.0-rc.1")]
        );
    }

    #[test]
    fn channel_tags_skips_channels_without_a_tag() {
        let info = info("", "v4.4.0-alpha.1");
        assert_eq!(channel_tags(&info), vec![("beta", "v4.4.0-alpha.1")]);
    }
}
