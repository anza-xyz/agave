use {
    super::resolve::{Xy, parse_xy, xy_branch},
    anyhow::{Result, anyhow},
    futures_util::TryStreamExt,
    semver::Version,
    serde::Deserialize,
    std::env,
    tokio::pin,
};

const OWNER: &str = "anza-xyz";
const REPO: &str = "agave";

pub fn github_client() -> Result<octocrab::Octocrab> {
    let builder = match env::var("GH_TOKEN") {
        Ok(token) if !token.trim().is_empty() => {
            octocrab::Octocrab::builder().personal_token(token)
        }
        _ => {
            log::warn!("`GH_TOKEN` is not set; using unauthenticated GitHub client");
            octocrab::Octocrab::builder()
        }
    };

    Ok(builder.build()?)
}

pub async fn release_heads(client: &octocrab::Octocrab) -> Result<Vec<Xy>> {
    let page = client.repos(OWNER, REPO).list_branches().send().await?;
    let stream = page.into_stream(client);
    pin!(stream);

    let mut heads = vec![];
    while let Some(branch) = stream.try_next().await? {
        if let Some(xy) = parse_xy(&branch.name) {
            heads.push(xy);
        }
    }

    Ok(heads)
}

pub async fn release_tags(client: &octocrab::Octocrab) -> Result<Vec<Version>> {
    let page = client.repos(OWNER, REPO).list_tags().send().await?;
    let stream = page.into_stream(client);
    pin!(stream);

    let mut tags = vec![];
    while let Some(tag) = stream.try_next().await? {
        let Some(stripped) = tag.name.strip_prefix('v') else {
            continue;
        };
        let Ok(v) = Version::parse(stripped) else {
            continue;
        };
        if v.pre.is_empty() && v.build.is_empty() {
            tags.push(v);
        }
    }

    Ok(tags)
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

pub async fn workspace_version(client: &octocrab::Octocrab, xy: Xy) -> Result<Version> {
    let reference = xy_branch(xy);
    let content_items = client
        .repos(OWNER, REPO)
        .get_content()
        .path("Cargo.toml")
        .r#ref(&reference)
        .send()
        .await?;

    let item = content_items
        .items
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Cargo.toml not found at ref {reference}"))?;
    let raw = item
        .decoded_content()
        .ok_or_else(|| anyhow!("Cargo.toml at ref {reference} has no decodable content"))?;

    let parsed: CargoToml = toml::from_str(&raw)
        .map_err(|e| anyhow!("failed to parse Cargo.toml at ref {reference}: {e}"))?;
    Ok(parsed.workspace.package.version)
}
