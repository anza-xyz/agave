use {
    anyhow::{Result, anyhow, bail},
    semver::Version,
    serde::Serialize,
    std::{collections::BTreeMap, fmt, str::FromStr},
};

/// `vX.Y` release-line identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct BranchVersion {
    pub major: u64,
    pub minor: u64,
}

impl FromStr for BranchVersion {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let rest = s
            .strip_prefix('v')
            .ok_or_else(|| anyhow!("missing `v` prefix in `{s}`"))?;
        let (maj, min) = rest
            .split_once('.')
            .ok_or_else(|| anyhow!("missing `.` separator in `{s}`"))?;
        Ok(Self {
            major: maj.parse()?,
            minor: min.parse()?,
        })
    }
}

impl fmt::Display for BranchVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}", self.major, self.minor)
    }
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

/// Manual channel pins parsed from `ci/channel-overrides` on master. A set pin
/// forces the corresponding channel to a specific `vX.Y` line, overriding the
/// version-derived value; `None` leaves auto-resolution in place.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChannelPins {
    pub beta: Option<BranchVersion>,
    pub stable: Option<BranchVersion>,
}

impl ChannelPins {
    pub fn parse(contents: &str) -> Result<Self> {
        Ok(Self {
            beta: extract_pin(contents, "PINNED_BETA_CHANNEL")?,
            stable: extract_pin(contents, "PINNED_STABLE_CHANNEL")?,
        })
    }
}

fn extract_pin(contents: &str, key: &str) -> Result<Option<BranchVersion>> {
    for line in contents.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        if v.is_empty() {
            return Ok(None);
        }
        return v
            .parse::<BranchVersion>()
            .map(Some)
            .map_err(|e| anyhow!("invalid {key} pin `{v}`: {e}"));
    }
    Ok(None)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
    versions: &BTreeMap<BranchVersion, Version>,
    tags: &[Version],
    branch: Option<&str>,
    channel: Option<&str>,
    pins: &ChannelPins,
) -> Result<ChannelInfo> {
    for (bv, v) in versions {
        if v.major != bv.major || v.minor != bv.minor {
            bail!("branch {bv} workspace version {v} does not match branch (major, minor)");
        }
    }

    let sorted: Vec<BranchVersion> = versions.keys().rev().copied().collect();
    let h1 = sorted.first().copied();
    let h2 = sorted.get(1).copied();
    let h3 = sorted.get(2).copied();

    let h1_stage = h1
        .map(|bv| stage_of(versions.get(&bv).expect("h1 is in versions")))
        .transpose()?;

    let (beta, stable) = match h1_stage {
        Some(Stage::Alpha) => (h2, h3),
        Some(_) => (h1, h2),
        None => (None, None),
    };

    let beta = pins.beta.or(beta);
    let stable = pins.stable.or(stable);

    let beta = beta.ok_or_else(|| anyhow!("no BETA-eligible vX.Y head"))?;

    if let Some(s) = stable
        && s >= beta
    {
        bail!("STABLE {s} is not less than BETA {beta}");
    }

    let edge_channel = "master".to_string();
    let beta_channel = beta.to_string();
    let stable_channel = stable.map(|s| s.to_string()).unwrap_or_default();
    let beta_channel_latest_tag = latest_tag_for(tags, beta)
        .map(|v| format!("v{v}"))
        .unwrap_or_default();
    let stable_channel_latest_tag = stable
        .and_then(|xy| latest_tag_for(tags, xy))
        .map(|v| format!("v{v}"))
        .unwrap_or_default();

    let channel = channel.map(str::to_owned).unwrap_or_else(|| match branch {
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

fn latest_tag_for(tags: &[Version], bv: BranchVersion) -> Option<&Version> {
    tags.iter()
        .filter(|t| t.major == bv.major && t.minor == bv.minor)
        .max()
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

pub fn print_channel_info_json(info: &ChannelInfo) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(info)?);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).expect("valid version")
    }

    fn bv(major: u64, minor: u64) -> BranchVersion {
        BranchVersion { major, minor }
    }

    fn versions(pairs: &[(BranchVersion, &str)]) -> BTreeMap<BranchVersion, Version> {
        pairs.iter().map(|(b, s)| (*b, v(s))).collect()
    }

    #[test]
    fn promotes_top_when_top_is_beta() {
        let vs = versions(&[(bv(4, 0), "4.0.0"), (bv(4, 1), "4.1.0-beta.0")]);

        let info = derive_channels(&vs, &[], None, None, &ChannelPins::default()).unwrap();

        assert_eq!(info.beta_channel, "v4.1");
        assert_eq!(info.stable_channel, "v4.0");
    }

    #[test]
    fn promotes_top_when_top_is_ga() {
        let vs = versions(&[(bv(4, 0), "4.0.5"), (bv(4, 1), "4.1.0")]);

        let info = derive_channels(&vs, &[], None, None, &ChannelPins::default()).unwrap();

        assert_eq!(info.beta_channel, "v4.1");
        assert_eq!(info.stable_channel, "v4.0");
    }

    #[test]
    fn holds_channels_when_top_is_alpha() {
        let vs = versions(&[
            (bv(3, 1), "3.1.15"),
            (bv(4, 0), "4.0.0"),
            (bv(4, 1), "4.1.0-alpha.0"),
        ]);

        let info = derive_channels(&vs, &[], None, None, &ChannelPins::default()).unwrap();

        assert_eq!(info.beta_channel, "v4.0");
        assert_eq!(info.stable_channel, "v3.1");
    }

    #[test]
    fn rc_top_is_promoted() {
        let vs = versions(&[(bv(4, 0), "4.0.10"), (bv(4, 1), "4.1.0-rc.2")]);

        let info = derive_channels(&vs, &[], None, None, &ChannelPins::default()).unwrap();

        assert_eq!(info.beta_channel, "v4.1");
        assert_eq!(info.stable_channel, "v4.0");
    }

    #[test]
    fn rejects_mismatched_workspace_version() {
        let vs = versions(&[(bv(4, 0), "5.0.0")]);

        let err = derive_channels(&vs, &[], None, None, &ChannelPins::default()).unwrap_err();

        assert!(err.to_string().contains("does not match branch"));
    }

    #[test]
    fn rejects_unknown_prerelease_label() {
        let vs = versions(&[(bv(4, 0), "4.0.0"), (bv(4, 1), "4.1.0-dev.0")]);

        let err = derive_channels(&vs, &[], None, None, &ChannelPins::default()).unwrap_err();

        assert!(err.to_string().contains("unknown prerelease label"));
    }

    #[test]
    fn rejects_when_only_alpha_top_and_nothing_below() {
        let vs = versions(&[(bv(4, 1), "4.1.0-alpha.0")]);

        let err = derive_channels(&vs, &[], None, None, &ChannelPins::default()).unwrap_err();

        assert!(err.to_string().contains("no BETA-eligible"));
    }

    #[test]
    fn rejects_empty() {
        let err = derive_channels(&BTreeMap::new(), &[], None, None, &ChannelPins::default())
            .unwrap_err();

        assert!(err.to_string().contains("no BETA-eligible"));
    }

    #[test]
    fn picks_latest_tag_per_channel() {
        let vs = versions(&[(bv(3, 0), "3.0.5"), (bv(3, 1), "3.1.0-beta.0")]);
        let tags = vec![
            v("3.0.1"),
            v("3.0.5"),
            v("3.0.6"),
            v("3.1.0-alpha.0"),
            v("3.1.0-alpha.1"),
            v("3.1.0-beta.0"),
            v("3.1.0-beta.1"),
            v("2.9.9"),
        ];

        let info = derive_channels(&vs, &tags, None, None, &ChannelPins::default()).unwrap();

        assert_eq!(info.beta_channel_latest_tag, "v3.1.0-beta.1");
        assert_eq!(info.stable_channel_latest_tag, "v3.0.6");
    }

    #[test]
    fn channel_from_branch_match() {
        let vs = versions(&[(bv(4, 0), "4.0.0"), (bv(4, 1), "4.1.0-beta.0")]);

        let info = derive_channels(&vs, &[], Some("v4.1"), None, &ChannelPins::default()).unwrap();

        assert_eq!(info.channel, "beta");
    }

    #[test]
    fn channel_env_var_wins() {
        let vs = versions(&[(bv(4, 0), "4.0.0"), (bv(4, 1), "4.1.0-beta.0")]);

        let info = derive_channels(
            &vs,
            &[],
            Some("master"),
            Some("stable"),
            &ChannelPins::default(),
        )
        .unwrap();

        assert_eq!(info.channel, "stable");
    }

    #[test]
    fn json_uses_screaming_snake_case_keys() {
        let vs = versions(&[(bv(4, 0), "4.0.0"), (bv(4, 1), "4.1.0-beta.0")]);
        let info = derive_channels(&vs, &[], None, None, &ChannelPins::default()).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&info).unwrap()).unwrap();

        assert_eq!(json["EDGE_CHANNEL"], "master");
        assert_eq!(json["BETA_CHANNEL"], "v4.1");
        assert_eq!(json["STABLE_CHANNEL"], "v4.0");
        assert_eq!(json["BETA_CHANNEL_LATEST_TAG"], "");
        assert_eq!(json["STABLE_CHANNEL_LATEST_TAG"], "");
        assert_eq!(json["CHANNEL"], "");
        assert_eq!(json["CHANNEL_LATEST_TAG"], "");
    }

    #[test]
    fn pin_overrides_auto_derived_channels() {
        let vs = versions(&[
            (bv(4, 0), "4.0.0"),
            (bv(4, 1), "4.1.0"),
            (bv(4, 2), "4.2.0-beta.0"),
        ]);
        let pins = ChannelPins {
            beta: Some(bv(4, 1)),
            stable: Some(bv(4, 0)),
        };

        let info = derive_channels(&vs, &[], None, None, &pins).unwrap();

        assert_eq!(info.beta_channel, "v4.1");
        assert_eq!(info.stable_channel, "v4.0");
    }

    #[test]
    fn empty_pins_leave_auto_resolution_intact() {
        let vs = versions(&[(bv(4, 0), "4.0.0"), (bv(4, 1), "4.1.0-beta.0")]);

        let info = derive_channels(&vs, &[], None, None, &ChannelPins::default()).unwrap();

        assert_eq!(info.beta_channel, "v4.1");
        assert_eq!(info.stable_channel, "v4.0");
    }

    #[test]
    fn pinned_channel_picks_its_own_latest_tag() {
        let vs = versions(&[(bv(4, 0), "4.0.0"), (bv(4, 1), "4.1.0-beta.0")]);
        let tags = vec![v("4.0.3"), v("4.0.1")];
        let pins = ChannelPins {
            beta: None,
            stable: Some(bv(4, 0)),
        };

        let info = derive_channels(&vs, &tags, None, None, &pins).unwrap();

        assert_eq!(info.stable_channel, "v4.0");
        assert_eq!(info.stable_channel_latest_tag, "v4.0.3");
    }

    #[test]
    fn parse_reads_set_and_empty_pins() {
        let contents = "\
# comment line
PINNED_BETA_CHANNEL=v4.1
PINNED_STABLE_CHANNEL=
";
        let pins = ChannelPins::parse(contents).unwrap();

        assert_eq!(pins.beta, Some(bv(4, 1)));
        assert_eq!(pins.stable, None);
    }

    #[test]
    fn parse_absent_keys_are_none() {
        let pins = ChannelPins::parse("# nothing here\n").unwrap();

        assert_eq!(pins, ChannelPins::default());
    }

    #[test]
    fn parse_rejects_malformed_pin() {
        let err = ChannelPins::parse("PINNED_BETA_CHANNEL=4.1\n").unwrap_err();

        assert!(err.to_string().contains("invalid PINNED_BETA_CHANNEL pin"));
    }

    #[test]
    fn stage_of_classifies_known_labels() {
        assert_eq!(stage_of(&v("4.0.0")).unwrap(), Stage::Ga);
        assert_eq!(stage_of(&v("4.0.0-alpha.0")).unwrap(), Stage::Alpha);
        assert_eq!(stage_of(&v("4.0.0-beta.3")).unwrap(), Stage::Beta);
        assert_eq!(stage_of(&v("4.0.0-rc.1")).unwrap(), Stage::Rc);
    }
}
