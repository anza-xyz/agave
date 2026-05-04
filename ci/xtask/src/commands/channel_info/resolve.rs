use {
    anyhow::{Result, anyhow, bail},
    semver::Version,
    std::collections::BTreeMap,
};

/// `(major, minor)` pair identifying a `vX.Y` release line.
pub type Xy = (u64, u64);

pub fn parse_xy(s: &str) -> Option<Xy> {
    let s = s.strip_prefix('v')?;
    let (maj, min) = s.split_once('.')?;
    Some((maj.parse().ok()?, min.parse().ok()?))
}

pub fn xy_branch(xy: Xy) -> String {
    format!("v{}.{}", xy.0, xy.1)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Alpha,
    Beta,
    Rc,
    Ga,
}

pub fn stage_of(v: &Version) -> Result<Stage> {
    if v.pre.is_empty() {
        return Ok(Stage::Ga);
    }

    let label = v.pre.as_str().split('.').next().unwrap_or("");

    match label {
        "alpha" => Ok(Stage::Alpha),
        "beta" => Ok(Stage::Beta),
        "rc" => Ok(Stage::Rc),
        other => bail!("unknown prerelease label `{other}` in version {v}"),
    }
}

#[derive(Default)]
pub struct EnvInputs {
    pub branch: Option<String>,
    pub channel: Option<String>,
}

#[derive(Debug)]
pub struct ChannelInfo {
    pub edge_channel: String,
    pub beta_channel: String,
    pub beta_channel_latest_tag: String,
    pub stable_channel: String,
    pub stable_channel_latest_tag: String,
    pub channel: String,
    pub channel_latest_tag: String,
}

pub fn derive_channels(
    versions: &BTreeMap<Xy, Version>,
    tags: &[Version],
    env_in: &EnvInputs,
) -> Result<ChannelInfo> {
    for (xy, v) in versions {
        if (v.major, v.minor) != *xy {
            bail!(
                "branch {} workspace version {v} does not match branch (major, minor)",
                xy_branch(*xy),
            );
        }
    }

    let sorted: Vec<Xy> = versions.keys().rev().copied().collect();
    let h1 = sorted.first().copied();
    let h2 = sorted.get(1).copied();
    let h3 = sorted.get(2).copied();

    let h1_stage = h1
        .map(|xy| stage_of(versions.get(&xy).expect("h1 is in versions")))
        .transpose()?;

    let (beta, stable) = match h1_stage {
        Some(Stage::Alpha) => (h2, h3),
        Some(_) => (h1, h2),
        None => (None, None),
    };

    let beta = beta.ok_or_else(|| anyhow!("no BETA-eligible vX.Y head"))?;

    if let Some(s) = stable
        && s >= beta
    {
        bail!(
            "STABLE {} is not less than BETA {}",
            xy_branch(s),
            xy_branch(beta),
        );
    }

    let edge_channel = "master".to_string();
    let beta_channel = xy_branch(beta);
    let stable_channel = stable.map(xy_branch).unwrap_or_default();
    let beta_channel_latest_tag = latest_tag_for(tags, beta)
        .map(|v| format!("v{v}"))
        .unwrap_or_default();
    let stable_channel_latest_tag = stable
        .and_then(|xy| latest_tag_for(tags, xy))
        .map(|v| format!("v{v}"))
        .unwrap_or_default();

    let channel = env_in
        .channel
        .clone()
        .unwrap_or_else(|| match env_in.branch.as_deref() {
            Some(b) if b == stable_channel => "stable".into(),
            Some(b) if b == edge_channel => "edge".into(),
            Some(b) if b == beta_channel => "beta".into(),
            _ => String::new(),
        });

    let channel_latest_tag = match channel.as_str() {
        "beta" => beta_channel_latest_tag.clone(),
        "stable" => stable_channel_latest_tag.clone(),
        _ => String::new(),
    };

    Ok(ChannelInfo {
        edge_channel,
        beta_channel,
        beta_channel_latest_tag,
        stable_channel,
        stable_channel_latest_tag,
        channel,
        channel_latest_tag,
    })
}

fn latest_tag_for(tags: &[Version], xy: Xy) -> Option<&Version> {
    tags.iter().filter(|t| (t.major, t.minor) == xy).max()
}

pub fn print_channel_info(info: &ChannelInfo) {
    println!("EDGE_CHANNEL={}", info.edge_channel);
    println!("BETA_CHANNEL={}", info.beta_channel);
    println!("BETA_CHANNEL_LATEST_TAG={}", info.beta_channel_latest_tag);
    println!("STABLE_CHANNEL={}", info.stable_channel);
    println!(
        "STABLE_CHANNEL_LATEST_TAG={}",
        info.stable_channel_latest_tag
    );
    println!("CHANNEL={}", info.channel);
    println!("CHANNEL_LATEST_TAG={}", info.channel_latest_tag);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).expect("valid version")
    }

    fn versions(pairs: &[(Xy, &str)]) -> BTreeMap<Xy, Version> {
        pairs.iter().map(|(xy, s)| (*xy, v(s))).collect()
    }

    #[test]
    fn promotes_top_when_top_is_beta() {
        let vs = versions(&[((4, 0), "4.0.0"), ((4, 1), "4.1.0-beta.0")]);

        let info = derive_channels(&vs, &[], &EnvInputs::default()).unwrap();

        assert_eq!(info.beta_channel, "v4.1");
        assert_eq!(info.stable_channel, "v4.0");
    }

    #[test]
    fn promotes_top_when_top_is_ga() {
        let vs = versions(&[((4, 0), "4.0.5"), ((4, 1), "4.1.0")]);

        let info = derive_channels(&vs, &[], &EnvInputs::default()).unwrap();

        assert_eq!(info.beta_channel, "v4.1");
        assert_eq!(info.stable_channel, "v4.0");
    }

    #[test]
    fn holds_channels_when_top_is_alpha() {
        let vs = versions(&[
            ((3, 1), "3.1.15"),
            ((4, 0), "4.0.0"),
            ((4, 1), "4.1.0-alpha.0"),
        ]);

        let info = derive_channels(&vs, &[], &EnvInputs::default()).unwrap();

        assert_eq!(info.beta_channel, "v4.0");
        assert_eq!(info.stable_channel, "v3.1");
    }

    #[test]
    fn rc_top_is_promoted() {
        let vs = versions(&[((4, 0), "4.0.10"), ((4, 1), "4.1.0-rc.2")]);

        let info = derive_channels(&vs, &[], &EnvInputs::default()).unwrap();

        assert_eq!(info.beta_channel, "v4.1");
        assert_eq!(info.stable_channel, "v4.0");
    }

    #[test]
    fn rejects_mismatched_workspace_version() {
        let vs = versions(&[((4, 0), "5.0.0")]);

        let err = derive_channels(&vs, &[], &EnvInputs::default()).unwrap_err();

        assert!(err.to_string().contains("does not match branch"));
    }

    #[test]
    fn rejects_unknown_prerelease_label() {
        let vs = versions(&[((4, 0), "4.0.0"), ((4, 1), "4.1.0-dev.0")]);

        let err = derive_channels(&vs, &[], &EnvInputs::default()).unwrap_err();

        assert!(err.to_string().contains("unknown prerelease label"));
    }

    #[test]
    fn rejects_when_only_alpha_top_and_nothing_below() {
        let vs = versions(&[((4, 1), "4.1.0-alpha.0")]);

        let err = derive_channels(&vs, &[], &EnvInputs::default()).unwrap_err();

        assert!(err.to_string().contains("no BETA-eligible"));
    }

    #[test]
    fn rejects_empty() {
        let err = derive_channels(&BTreeMap::new(), &[], &EnvInputs::default()).unwrap_err();

        assert!(err.to_string().contains("no BETA-eligible"));
    }

    #[test]
    fn picks_latest_tag_per_channel() {
        let vs = versions(&[((3, 0), "3.0.5"), ((3, 1), "3.1.0-beta.0")]);
        let tags = vec![v("3.0.1"), v("3.0.5"), v("3.0.2"), v("2.9.9")];

        let info = derive_channels(&vs, &tags, &EnvInputs::default()).unwrap();

        assert_eq!(info.beta_channel_latest_tag, "");
        assert_eq!(info.stable_channel_latest_tag, "v3.0.5");
    }

    #[test]
    fn channel_from_branch_match() {
        let vs = versions(&[((4, 0), "4.0.0"), ((4, 1), "4.1.0-beta.0")]);
        let env_in = EnvInputs {
            branch: Some("v4.1".into()),
            channel: None,
        };

        let info = derive_channels(&vs, &[], &env_in).unwrap();

        assert_eq!(info.channel, "beta");
    }

    #[test]
    fn channel_env_var_wins() {
        let vs = versions(&[((4, 0), "4.0.0"), ((4, 1), "4.1.0-beta.0")]);
        let env_in = EnvInputs {
            branch: Some("master".into()),
            channel: Some("stable".into()),
        };

        let info = derive_channels(&vs, &[], &env_in).unwrap();

        assert_eq!(info.channel, "stable");
    }

    #[test]
    fn stage_of_classifies_known_labels() {
        assert_eq!(stage_of(&v("4.0.0")).unwrap(), Stage::Ga);
        assert_eq!(stage_of(&v("4.0.0-alpha.0")).unwrap(), Stage::Alpha);
        assert_eq!(stage_of(&v("4.0.0-beta.3")).unwrap(), Stage::Beta);
        assert_eq!(stage_of(&v("4.0.0-rc.1")).unwrap(), Stage::Rc);
    }
}
