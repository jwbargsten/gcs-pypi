//! Access tokens from the VM's metadata server or `gcloud`, cached until shortly before expiry.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AuthMode {
    Auto,
    Metadata,
    Gcloud,
    None,
}

struct CachedToken {
    value: String,
    refresh_at: Instant,
}

pub struct Auth {
    mode: AuthMode,
    metadata_http: reqwest::Client,
    cached: Mutex<Option<CachedToken>>,
}

#[derive(Deserialize)]
struct MetadataToken {
    access_token: String,
    expires_in: u64,
}

impl Auth {
    pub fn new(mode: AuthMode) -> Result<Self> {
        // The metadata server must be reached directly, never through HTTP(S)_PROXY.
        let metadata_http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()?;
        Ok(Self {
            mode,
            metadata_http,
            cached: Mutex::new(None),
        })
    }

    pub async fn token(&self) -> Result<Option<String>> {
        if self.mode == AuthMode::None {
            return Ok(None);
        }
        let mut cached = self.cached.lock().await;
        if let Some(t) = cached.as_ref()
            && Instant::now() < t.refresh_at
        {
            return Ok(Some(t.value.clone()));
        }
        let (value, lifetime) = match self.mode {
            AuthMode::Metadata => self.from_metadata().await?,
            AuthMode::Gcloud => from_gcloud().await?,
            AuthMode::Auto => match self.from_metadata().await {
                Ok(t) => t,
                Err(meta_err) => from_gcloud()
                    .await
                    .map_err(|g| anyhow!("no credentials: metadata server: {meta_err:#}; gcloud: {g:#}"))?,
            },
            AuthMode::None => unreachable!(),
        };
        // Refresh a minute before expiry.
        let refresh_at = Instant::now() + lifetime.saturating_sub(Duration::from_secs(60));
        *cached = Some(CachedToken {
            value: value.clone(),
            refresh_at,
        });
        Ok(Some(value))
    }

    async fn from_metadata(&self) -> Result<(String, Duration)> {
        let host =
            std::env::var("GCE_METADATA_HOST").unwrap_or_else(|_| "metadata.google.internal".to_string());
        let url = format!("http://{host}/computeMetadata/v1/instance/service-accounts/default/token");
        let t: MetadataToken = self
            .metadata_http
            .get(url)
            .header("Metadata-Flavor", "Google")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok((t.access_token, Duration::from_secs(t.expires_in)))
    }
}

async fn from_gcloud() -> Result<(String, Duration)> {
    let out = tokio::process::Command::new("gcloud")
        .args(["auth", "print-access-token"])
        .output()
        .await
        .context("running gcloud")?;
    if !out.status.success() {
        bail!("gcloud failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let token = String::from_utf8(out.stdout)?.trim().to_string();
    // gcloud tokens live ~60 min; assume less since we don't know when it was minted.
    Ok((token, Duration::from_secs(30 * 60)))
}
