use {
    super::channel_info,
    crate::release_bucket::{DOWNLOAD_ROOT, ReleaseBucket, install_script},
    anyhow::Result,
    clap::Args,
    log::info,
    std::env,
};

#[derive(Args)]
pub struct CommandArgs {
    /// Log the upload instead of performing it
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: CommandArgs) -> Result<()> {
    if env::var("DO_NOT_PUBLISH_TAR").is_ok_and(|v| !v.is_empty()) {
        info!("skipping install script, DO_NOT_PUBLISH_TAR is set");
        return Ok(());
    }

    let release = match env::var("CI_TAG").ok().filter(|tag| !tag.is_empty()) {
        Some(tag) => tag,
        None => channel_info::resolve().await?.channel,
    };

    match release.as_str() {
        "" => info!("not a tag or channel build, nothing to publish"),
        "stable" | "beta" => {
            info!("{release} install script is published by `cargo xtask sync-release-channels`")
        }
        _ => {
            ReleaseBucket::new(args.dry_run)?
                .upload(install_script(&release)?, &format!("{release}/install"))
                .await?;
            if !args.dry_run {
                info!("published to {DOWNLOAD_ROOT}/{release}/install");
            }
        }
    }

    Ok(())
}
