use {
    super::resolve::{BranchVersion, ChannelPins},
    anyhow::{Result, anyhow, bail},
    semver::Version,
    serde::Deserialize,
    std::{
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    },
};

const REMOTE: &str = "https://github.com/anza-xyz/agave.git";
const RAW_BASE: &str = "https://raw.githubusercontent.com/anza-xyz/agave";

fn ls_remote(flag: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["ls-remote", flag, REMOTE])
        .output()
        .map_err(|e| anyhow!("failed to invoke `git ls-remote`: {e}"))?;
    if !output.status.success() {
        bail!(
            "`git ls-remote {flag} {REMOTE}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| anyhow!("`git ls-remote` stdout is not utf-8: {e}"))?;
    Ok(stdout.lines().map(str::to_owned).collect())
}

fn strip_ref(line: &str, prefix: &str) -> Option<String> {
    let (_sha, refname) = line.split_once('\t')?;
    refname.strip_prefix(prefix).map(str::to_owned)
}

pub fn release_heads() -> Result<Vec<BranchVersion>> {
    let lines = ls_remote("--heads")?;
    Ok(lines
        .iter()
        .filter_map(|l| strip_ref(l, "refs/heads/"))
        .filter_map(|name| name.parse::<BranchVersion>().ok())
        .collect())
}

pub fn release_tags() -> Result<Vec<Version>> {
    let lines = ls_remote("--tags")?;
    Ok(lines
        .iter()
        .filter_map(|l| strip_ref(l, "refs/tags/"))
        .filter_map(|name| parse_release_tag(&name))
        .collect())
}

fn parse_release_tag(name: &str) -> Option<Version> {
    let stripped = name.strip_prefix('v')?;
    let version = Version::parse(stripped).ok()?;
    Some(version)
}

#[derive(Deserialize)]
struct CargoToml {
    workspace: WorkspaceSection,
}

#[derive(Deserialize)]
struct WorkspaceSection {
    package: PackageSection,
}

#[derive(Deserialize)]
struct PackageSection {
    version: Version,
}

/// Fetch pin overrides from `master` (single source of truth for all
/// branches). Cache-busted so a pin change takes effect at once; transport
/// errors hard-fail rather than silently falling back to auto-resolution.
pub async fn channel_pins(client: &reqwest::Client) -> Result<ChannelPins> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let url = format!("{RAW_BASE}/master/ci/channel-overrides");
    let resp = client
        .get(&url)
        .query(&[("ts", ts)])
        .send()
        .await
        .map_err(|e| anyhow!("GET {url}: {e}"))?
        .error_for_status()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let raw = resp
        .text()
        .await
        .map_err(|e| anyhow!("read body for {url}: {e}"))?;
    ChannelPins::parse(&raw)
}

pub async fn workspace_version(client: &reqwest::Client, bv: BranchVersion) -> Result<Version> {
    let url = format!("{RAW_BASE}/{bv}/Cargo.toml");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("GET {url}: {e}"))?
        .error_for_status()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let raw = resp
        .text()
        .await
        .map_err(|e| anyhow!("read body for {url}: {e}"))?;
    let parsed: CargoToml =
        toml::from_str(&raw).map_err(|e| anyhow!("failed to parse Cargo.toml at {url}: {e}"))?;
    Ok(parsed.workspace.package.version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_final_and_prerelease_tags() {
        assert_eq!(parse_release_tag("v4.2.0"), Version::parse("4.2.0").ok());
        assert_eq!(
            parse_release_tag("v4.2.0-beta.1"),
            Version::parse("4.2.0-beta.1").ok()
        );
    }

    #[test]
    fn parses_build_tags_and_rejects_non_release_tags() {
        assert_eq!(
            parse_release_tag("v4.2.0+build.1"),
            Version::parse("4.2.0+build.1").ok()
        );
        assert_eq!(parse_release_tag("4.2.0"), None);
        assert_eq!(parse_release_tag("v4.2.0^{}"), None);
    }
}
