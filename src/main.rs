//! gcs-pypi: serve a GCS bucket of Python distributions as a local PEP 503 "simple" index.
//!
//! The bucket needs no index files: wheels/sdists are dropped anywhere under the prefix and
//! the index is generated on the fly from the object listing. Auth uses the VM's metadata
//! server (Vertex Workbench / GCE / GKE), falling back to `gcloud` for local use.
//!
//!   gcs-pypi --bucket gs://my-pypi/packages -- pip install mylib
//!   gcs-pypi --bucket gs://my-pypi/packages -- uv sync

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use base64::Engine;
use clap::Parser;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Deserialize;
use tokio::sync::Mutex;

/// Encode everything except RFC 3986 unreserved characters.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

#[derive(Parser, Debug)]
#[command(
    name = "gcs-pypi",
    version,
    about = "Serve a GCS bucket as a local Python package index for pip/uv",
    after_help = "With a command after `--`, the server starts on a free port, PIP_INDEX_URL and \
                  UV_DEFAULT_INDEX are set for the command, and the server stops when it exits.\n\
                  Without a command, the server runs until interrupted."
)]
struct Args {
    /// Bucket and optional prefix, e.g. gs://my-pypi or gs://my-pypi/packages
    #[arg(long, env = "GCS_PYPI_BUCKET")]
    bucket: String,

    /// Address to listen on (port 0 picks a free port)
    #[arg(long, env = "GCS_PYPI_ADDR", default_value = "127.0.0.1:0")]
    addr: SocketAddr,

    /// Simple index to redirect to for projects NOT in the bucket ("none" to disable).
    /// Projects that exist in the bucket are never looked up upstream.
    #[arg(long, env = "GCS_PYPI_UPSTREAM", default_value = "https://pypi.org/simple/")]
    upstream: String,

    /// Seconds to cache the bucket listing
    #[arg(long, env = "GCS_PYPI_CACHE_TTL", default_value_t = 60)]
    cache_ttl: u64,

    /// Auth source: auto (metadata server, then gcloud), metadata, gcloud, none
    #[arg(long, env = "GCS_PYPI_AUTH", default_value = "auto")]
    auth: String,

    /// GCS API endpoint (override for emulators)
    #[arg(long, env = "GCS_PYPI_ENDPOINT", default_value = "https://storage.googleapis.com")]
    endpoint: String,

    /// Log each request to stderr
    #[arg(long, short)]
    verbose: bool,

    /// Command to run with the index configured
    #[arg(last = true)]
    command: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum AuthMode {
    Auto,
    Metadata,
    Gcloud,
    None,
}

struct CachedToken {
    value: String,
    refresh_at: Instant,
}

struct Auth {
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
    fn new(mode: AuthMode) -> Result<Self> {
        // The metadata server must be reached directly, never through HTTP(S)_PROXY.
        let metadata_http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()?;
        Ok(Self { mode, metadata_http, cached: Mutex::new(None) })
    }

    async fn token(&self) -> Result<Option<String>> {
        if self.mode == AuthMode::None {
            return Ok(None);
        }
        let mut cached = self.cached.lock().await;
        if let Some(t) = cached.as_ref() {
            if Instant::now() < t.refresh_at {
                return Ok(Some(t.value.clone()));
            }
        }
        let (value, lifetime) = match self.mode {
            AuthMode::Metadata => self.from_metadata().await?,
            AuthMode::Gcloud => from_gcloud().await?,
            AuthMode::Auto => match self.from_metadata().await {
                Ok(t) => t,
                Err(meta_err) => from_gcloud().await.map_err(|g| {
                    anyhow!("no credentials: metadata server: {meta_err:#}; gcloud: {g:#}")
                })?,
            },
            AuthMode::None => unreachable!(),
        };
        // Refresh a minute before expiry.
        let refresh_at = Instant::now() + lifetime.saturating_sub(Duration::from_secs(60));
        *cached = Some(CachedToken { value: value.clone(), refresh_at });
        Ok(Some(value))
    }

    async fn from_metadata(&self) -> Result<(String, Duration)> {
        let host = std::env::var("GCE_METADATA_HOST")
            .unwrap_or_else(|_| "metadata.google.internal".to_string());
        let url = format!(
            "http://{host}/computeMetadata/v1/instance/service-accounts/default/token"
        );
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

// ---------------------------------------------------------------------------------------------
// Bucket listing -> index
// ---------------------------------------------------------------------------------------------

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
struct Dist {
    filename: String,
    object: String,
    /// (algorithm, hex digest) for the URL fragment pip/uv verify against.
    hash: Option<(&'static str, String)>,
    requires_python: Option<String>,
}

type Index = BTreeMap<String, Vec<Dist>>;

struct Listing {
    index: Arc<Index>,
    fetched_at: Instant,
}

struct AppState {
    http: reqwest::Client,
    auth: Auth,
    endpoint: String,
    bucket: String,
    prefix: String,
    upstream: Option<String>,
    ttl: Duration,
    verbose: bool,
    listing: Mutex<Option<Listing>>,
}

/// PEP 503 name normalization.
fn normalize(name: &str) -> String {
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
fn project_of(filename: &str) -> Option<String> {
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
    if let Some(sha) = obj.metadata.get("sha256") {
        if sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(("sha256", sha.to_ascii_lowercase()));
        }
    }
    let raw = base64::engine::general_purpose::STANDARD.decode(obj.md5_hash.as_ref()?).ok()?;
    Some(("md5", raw.iter().map(|b| format!("{b:02x}")).collect()))
}

impl AppState {
    async fn authed(&self, req: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
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
                bail!("listing gs://{}/{} failed: {status}: {body}", self.bucket, self.prefix);
            }
            let list: ListResponse = resp.json().await?;
            for obj in list.items {
                let filename = obj.name.rsplit('/').next().unwrap_or(&obj.name).to_string();
                let Some(project) = project_of(&filename) else { continue };
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

    async fn index(&self) -> Result<Arc<Index>> {
        let mut listing = self.listing.lock().await;
        if let Some(l) = listing.as_ref() {
            if l.fetched_at.elapsed() < self.ttl {
                return Ok(l.index.clone());
            }
        }
        let index = Arc::new(self.fetch_index().await?);
        *listing = Some(Listing { index: index.clone(), fetched_at: Instant::now() });
        Ok(index)
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn page(title: &str, links: &str) -> Html<String> {
    Html(format!(
        "<!DOCTYPE html>\n<html><head><meta name=\"pypi:repository-version\" content=\"1.0\">\
         <title>{}</title></head><body>\n{links}</body></html>\n",
        esc(title)
    ))
}

fn internal(e: anyhow::Error) -> Response {
    eprintln!("gcs-pypi: error: {e:#}");
    (StatusCode::BAD_GATEWAY, format!("gcs-pypi: {e:#}\n")).into_response()
}

async fn root_index(State(st): State<Arc<AppState>>) -> Response {
    let index = match st.index().await {
        Ok(i) => i,
        Err(e) => return internal(e),
    };
    let links: String = index
        .keys()
        .map(|p| format!("<a href=\"{0}/\">{0}</a><br/>\n", esc(p)))
        .collect();
    page("Simple index", &links).into_response()
}

async fn project_page(State(st): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let project = normalize(&name);
    let index = match st.index().await {
        Ok(i) => i,
        Err(e) => return internal(e),
    };
    let Some(dists) = index.get(&project) else {
        if st.verbose {
            eprintln!("gcs-pypi: {project}: not in bucket");
        }
        return match &st.upstream {
            Some(up) => Redirect::temporary(&format!("{up}{project}/")).into_response(),
            None => (StatusCode::NOT_FOUND, "not found\n").into_response(),
        };
    };
    let links: String = dists
        .iter()
        .map(|d| {
            let path: Vec<String> = d
                .object
                .split('/')
                .map(|seg| utf8_percent_encode(seg, COMPONENT).to_string())
                .collect();
            let frag = d.hash.as_ref().map(|(alg, hex)| format!("#{alg}={hex}")).unwrap_or_default();
            let rp = d
                .requires_python
                .as_ref()
                .map(|r| format!(" data-requires-python=\"{}\"", esc(r)))
                .unwrap_or_default();
            format!(
                "<a href=\"/files/{}{frag}\"{rp}>{}</a><br/>\n",
                path.join("/"),
                esc(&d.filename)
            )
        })
        .collect();
    page(&format!("Links for {project}"), &links).into_response()
}

async fn project_redirect(Path(name): Path<String>) -> Redirect {
    Redirect::permanent(&format!("/simple/{}/", utf8_percent_encode(&name, COMPONENT)))
}

async fn file(State(st): State<Arc<AppState>>, Path(object): Path<String>) -> Response {
    // Only serve objects that are distributions under the configured prefix.
    let filename = object.rsplit('/').next().unwrap_or(&object);
    if !object.starts_with(&st.prefix) || project_of(filename).is_none() {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    }
    let url = format!(
        "{}/storage/v1/b/{}/o/{}",
        st.endpoint,
        st.bucket,
        utf8_percent_encode(&object, COMPONENT)
    );
    let req = match st.authed(st.http.get(url).query(&[("alt", "media")])).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return internal(e.into()),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return internal(anyhow!("download gs://{}/{object}: {status}: {body}", st.bucket));
    }
    if st.verbose {
        eprintln!("gcs-pypi: GET {object}");
    }
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream");
    if let Some(len) = resp.content_length() {
        builder = builder.header(header::CONTENT_LENGTH, len);
    }
    builder
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|e| internal(e.into()))
}

// ---------------------------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------------------------

fn parse_bucket(spec: &str) -> Result<(String, String)> {
    let rest = spec.strip_prefix("gs://").unwrap_or(spec).trim_matches('/');
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    if bucket.is_empty() {
        bail!("invalid --bucket {spec:?}, expected gs://bucket[/prefix]");
    }
    let prefix = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
    Ok((bucket.to_string(), prefix))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (bucket, prefix) = parse_bucket(&args.bucket)?;
    let mode = match args.auth.as_str() {
        "auto" => AuthMode::Auto,
        "metadata" => AuthMode::Metadata,
        "gcloud" => AuthMode::Gcloud,
        "none" => AuthMode::None,
        other => bail!("unknown --auth {other:?} (auto|metadata|gcloud|none)"),
    };
    let upstream = match args.upstream.as_str() {
        "" | "none" => None,
        u => Some(if u.ends_with('/') { u.to_string() } else { format!("{u}/") }),
    };

    let state = Arc::new(AppState {
        http: reqwest::Client::builder().build()?,
        auth: Auth::new(mode)?,
        endpoint: args.endpoint.trim_end_matches('/').to_string(),
        bucket,
        prefix,
        upstream,
        ttl: Duration::from_secs(args.cache_ttl),
        verbose: args.verbose,
        listing: Mutex::new(None),
    });

    // Fail fast on auth/permission problems before pip/uv produce confusing errors.
    let n = state.index().await.context("initial bucket listing")?.len();

    let app = Router::new()
        .route("/simple/", get(root_index))
        .route("/simple/{name}/", get(project_page))
        .route("/simple/{name}", get(project_redirect))
        .route("/files/{*object}", get(file))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(args.addr).await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}/simple/");
    eprintln!(
        "gcs-pypi: serving gs://{}/{} ({n} projects) at {url}",
        state.bucket, state.prefix
    );

    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    if args.command.is_empty() {
        println!("PIP_INDEX_URL={url}");
        println!("UV_DEFAULT_INDEX={url}");
        tokio::select! {
            r = server => r??,
            _ = tokio::signal::ctrl_c() => {}
        }
        return Ok(());
    }

    let status = tokio::process::Command::new(&args.command[0])
        .args(&args.command[1..])
        .env("PIP_INDEX_URL", &url)
        .env("UV_DEFAULT_INDEX", &url)
        .env_remove("UV_INDEX_URL") // deprecated alias; would conflict with UV_DEFAULT_INDEX
        .status()
        .await
        .with_context(|| format!("running {:?}", args.command[0]))?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(normalize("Foo.Bar__baz"), "foo-bar-baz");
        assert_eq!(project_of("My_Lib-1.2.0-py3-none-any.whl").as_deref(), Some("my-lib"));
        assert_eq!(project_of("my-lib-1.2.0.tar.gz").as_deref(), Some("my-lib"));
        assert_eq!(project_of("my_lib-1.2.0.zip").as_deref(), Some("my-lib"));
        assert_eq!(project_of("README.md"), None);
        assert_eq!(project_of("notes.tar.gz"), None);
    }

    #[test]
    fn buckets() {
        assert_eq!(parse_bucket("gs://b").unwrap(), ("b".into(), "".into()));
        assert_eq!(parse_bucket("gs://b/a/c/").unwrap(), ("b".into(), "a/c/".into()));
    }
}
