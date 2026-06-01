use {
    super::resolve::BranchVersion,
    anyhow::{Result, anyhow, bail},
    semver::Version,
    serde::Deserialize,
    std::process::Command,
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
        .filter_map(|name| {
            let stripped = name.strip_prefix('v')?;
            let v = Version::parse(stripped).ok()?;
            (v.pre.is_empty() && v.build.is_empty()).then_some(v)
        })
        .collect())
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
    let parsed: CargoToml = toml::from_str(&raw)
        .map_err(|e| anyhow!("failed to parse Cargo.toml at {url}: {e}"))?;
    Ok(parsed.workspace.package.version)
}
