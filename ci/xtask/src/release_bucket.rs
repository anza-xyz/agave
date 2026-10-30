use {
    anyhow::{Context, Result},
    log::info,
    object_store::{
        ObjectStoreExt,
        gcp::{GoogleCloudStorage, GoogleCloudStorageBuilder},
        path::Path as ObjectPath,
    },
    std::{env, fs, path::Path},
};

const BUCKET: &str = "anza-release";
pub const DOWNLOAD_ROOT: &str = "https://release.anza.xyz";
const CREDENTIAL_ENV: &str = "GCS_RELEASE_BUCKET_WRITER_CREDIENTIAL";

pub fn install_script(release: &str) -> Result<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/agave-install-init.sh");
    let init_script =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;

    Ok(render_install_script(release, &init_script))
}

fn render_install_script(release: &str, init_script: &str) -> String {
    [
        format!("SOLANA_RELEASE={release}"),
        format!("SOLANA_INSTALL_INIT_ARGS={release}"),
        format!("SOLANA_DOWNLOAD_ROOT={DOWNLOAD_ROOT}"),
        init_script.to_owned(),
    ]
    .join("\n")
}

pub struct ReleaseBucket {
    store: Option<GoogleCloudStorage>,
}

impl ReleaseBucket {
    pub fn new(dry_run: bool) -> Result<Self> {
        if dry_run {
            return Ok(Self { store: None });
        }

        let credentials =
            env::var(CREDENTIAL_ENV).with_context(|| format!("{CREDENTIAL_ENV} is not set"))?;
        let store = GoogleCloudStorageBuilder::new()
            .with_bucket_name(BUCKET)
            .with_application_credentials(credentials)
            .build()
            .context("failed to configure release bucket client")?;

        Ok(Self { store: Some(store) })
    }

    pub async fn copy(&self, from: &str, to: &str) -> Result<()> {
        self.log(&format!("copy gs://{BUCKET}/{from} to gs://{BUCKET}/{to}"));
        if let Some(store) = &self.store {
            store
                .copy(&ObjectPath::from(from), &ObjectPath::from(to))
                .await
                .with_context(|| format!("failed to copy {from} to {to}"))?;
        }

        Ok(())
    }

    pub async fn upload(&self, data: String, to: &str) -> Result<()> {
        self.log(&format!("upload gs://{BUCKET}/{to}"));
        if let Some(store) = &self.store {
            store
                .put(&ObjectPath::from(to), data.into())
                .await
                .with_context(|| format!("failed to upload {to}"))?;
        }

        Ok(())
    }

    fn log(&self, action: &str) {
        if self.store.is_some() {
            info!("{action}");
        } else {
            info!("[dry-run] {action}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_header_names_the_release() {
        let script = render_install_script("stable", "#!/bin/sh\n");
        assert_eq!(
            script,
            "SOLANA_RELEASE=stable\nSOLANA_INSTALL_INIT_ARGS=stable\nSOLANA_DOWNLOAD_ROOT=https://release.anza.xyz\n#!/bin/sh\n"
        );
    }

    #[test]
    fn install_script_reads_init_script() {
        let script = install_script("v4.3.2").unwrap();
        assert!(script.starts_with("SOLANA_RELEASE=v4.3.2\n"));
        assert!(script.contains("agave-install-init"));
    }
}
