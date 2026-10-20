//! Bucket listing -> index: which distributions exist per (normalized) project name.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use base64::Engine;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::auth::Auth;

#[derive(Deserialize)]
struct ListResponse {
    #[serde(default)]
    items: Vec<GcsObject>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct GcsObject {
    name: String,
    #[serde(rename = "md5Hash")]
    md5_hash: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct Dist {
    pub filename: String,
    pub object: String,
    /// (algorithm, hex digest) for the URL fragment pip/uv verify against.
    pub hash: Option<(&'static str, String)>,
    pub requires_python: Option<String>,
}

pub type Index = BTreeMap<String, Vec<Dist>>;

pub struct Listing {
    index: Arc<Index>,
    fetched_at: Instant,
}

pub struct AppState {
    pub http: reqwest::Client,
    pub auth: Auth,
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
    pub upstream: Option<String>,
    pub ttl: Duration,
    pub verbose: bool,
    pub listing: Mutex<Option<Listing>>,
}

/// PEP 503 name normalization.
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_sep = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !prev_sep {
                out.push('-');
            }
            prev_sep = true;
        } else {
            out.push(c.to_ascii_lowercase());
            prev_sep = false;
        }
    }
    out
}

/// Project name from a distribution filename, or None if it's not a distribution.
pub fn project_of(filename: &str) -> Option<String> {
    if let Some(stem) = filename.strip_suffix(".whl") {
        // name-version(-build)?-pytag-abitag-plattag.whl ; name never contains '-'
        let name = stem.split('-').next()?;
        return (!name.is_empty() && stem.matches('-').count() >= 4).then(|| normalize(name));
    }
    for ext in [".tar.gz", ".zip"] {
        if let Some(stem) = filename.strip_suffix(ext) {
            // name-version ; older sdists may have '-' in the name, version never does
            let (name, version) = stem.rsplit_once('-')?;
            if name.is_empty() || !version.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            return Some(normalize(name));
        }
    }
    None
}

fn hash_for(obj: &GcsObject) -> Option<(&'static str, String)> {
    // Prefer a sha256 recorded at upload time (x-goog-meta-sha256); fall back to GCS's md5.
    if let Some(sha) = obj.metadata.get("sha256")
        && sha.len() == 64
        && sha.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Some(("sha256", sha.to_ascii_lowercase()));
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(obj.md5_hash.as_ref()?)
        .ok()?;
    Some(("md5", raw.iter().map(|b| format!("{b:02x}")).collect()))
}

impl AppState {
    pub async fn authed(&self, req: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        Ok(match self.auth.token().await? {
            Some(t) => req.bearer_auth(t),
            None => req,
        })
    }

    async fn fetch_index(&self) -> Result<Index> {
        let url = format!("{}/storage/v1/b/{}/o", self.endpoint, self.bucket);
        let mut index: Index = BTreeMap::new();
        let mut page: Option<String> = None;
        loop {
            let mut q = vec![
                ("prefix", self.prefix.clone()),
                ("fields", "items(name,md5Hash,metadata),nextPageToken".into()),
                ("maxResults", "1000".into()),
            ];
            if let Some(p) = &page {
                q.push(("pageToken", p.clone()));
            }
            let resp = self.authed(self.http.get(&url).query(&q)).await?.send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                bail!(
                    "listing gs://{}/{} failed: {status}: {body}",
                    self.bucket,
                    self.prefix
                );
            }
            let list: ListResponse = resp.json().await?;
            for obj in list.items {
                let filename = obj.name.rsplit('/').next().unwrap_or(&obj.name).to_string();
                let Some(project) = project_of(&filename) else {
                    continue;
                };
                index.entry(project).or_default().push(Dist {
                    hash: hash_for(&obj),
                    requires_python: obj.metadata.get("requires_python").cloned(),
                    filename,
                    object: obj.name,
                });
            }
            match list.next_page_token {
                Some(p) => page = Some(p),
                None => break,
            }
        }
        for dists in index.values_mut() {
            dists.sort_by(|a, b| a.filename.cmp(&b.filename));
        }
        Ok(index)
    }

    pub async fn index(&self) -> Result<Arc<Index>> {
        let mut listing = self.listing.lock().await;
        if let Some(l) = listing.as_ref()
            && l.fetched_at.elapsed() < self.ttl
        {
            return Ok(l.index.clone());
        }
        let index = Arc::new(self.fetch_index().await?);
        *listing = Some(Listing {
            index: index.clone(),
            fetched_at: Instant::now(),
        });
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("simple", "simple")]
    #[case("Foo.Bar__baz", "foo-bar-baz")]
    #[case("a--b", "a-b")]
    #[case("A-_.B", "a-b")]
    #[case("my_lib", "my-lib")]
    fn normalize_names(#[case] name: &str, #[case] expected: &str) {
        assert_eq!(normalize(name), expected);
    }

    #[rstest]
    #[case("My_Lib-1.2.0-py3-none-any.whl", Some("my-lib"))]
    #[case("pkg-1.0-1-py3-none-any.whl", Some("pkg"))]
    #[case("pkg-1.0-cp312-cp312-manylinux_2_17_x86_64.whl", Some("pkg"))]
    #[case("pkg-1.0.whl", None)]
    #[case("-1.0-py3-none-any.whl", None)]
    #[case("my-lib-1.2.0.tar.gz", Some("my-lib"))]
    #[case("my_lib-1.2.0.zip", Some("my-lib"))]
    #[case("notes.tar.gz", None)]
    #[case("notes-final.tar.gz", None)]
    #[case("-1.0.tar.gz", None)]
    #[case("pkg-1.0.tar.bz2", None)]
    #[case("README.md", None)]
    fn project_of_filenames(#[case] filename: &str, #[case] expected: Option<&str>) {
        assert_eq!(project_of(filename).as_deref(), expected);
    }

    const SHA: &str = "9F86D081884C7D659A2FEAA0C55AD015A3BF4F1B2B0B822CD15D6C15B0F00A08";
    /// md5 of the empty string, as GCS reports it (base64).
    const MD5_B64: &str = "1B2M2Y8AsgTpgAmY7PhCfg==";
    const MD5_HEX: &str = "d41d8cd98f00b204e9800998ecf8427e";
    const NON_HEX_SHA: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";

    fn obj(md5: Option<&str>, sha256: Option<&str>) -> GcsObject {
        GcsObject {
            name: "pkg-1.0.tar.gz".into(),
            md5_hash: md5.map(Into::into),
            metadata: sha256.map(|s| ("sha256".into(), s.into())).into_iter().collect(),
        }
    }

    #[rstest]
    #[case::sha256_preferred_and_lowercased(Some(MD5_B64), Some(SHA), Some(("sha256", SHA.to_ascii_lowercase())))]
    #[case::sha256_only(None, Some(SHA), Some(("sha256", SHA.to_ascii_lowercase())))]
    #[case::bad_sha256_falls_back_to_md5(Some(MD5_B64), Some("abc"), Some(("md5", MD5_HEX.into())))]
    #[case::non_hex_sha256_falls_back(Some(MD5_B64), Some(NON_HEX_SHA), Some(("md5", MD5_HEX.into())))]
    #[case::md5_only(Some(MD5_B64), None, Some(("md5", MD5_HEX.into())))]
    #[case::invalid_md5(Some("not base64!"), None, None)]
    #[case::no_hash(None, None, None)]
    fn hash_for_object(
        #[case] md5: Option<&str>,
        #[case] sha256: Option<&str>,
        #[case] expected: Option<(&'static str, String)>,
    ) {
        assert_eq!(hash_for(&obj(md5, sha256)), expected);
    }
}
