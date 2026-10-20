//! gcs-pypi: serve a GCS bucket of Python distributions as a local PEP 503 "simple" index.
//!
//! The bucket needs no index files: wheels/sdists are dropped anywhere under the prefix and
//! the index is generated on the fly from the object listing. Auth uses the VM's metadata
//! server (Vertex Workbench / GCE / GKE), falling back to `gcloud` for local use.
//!
//!   gcs-pypi --bucket gs://my-pypi/packages -- pip install mylib
//!   gcs-pypi --bucket gs://my-pypi/packages -- uv sync

mod auth;
mod daemon;
mod index;
mod server;

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::sync::Mutex;

use crate::{
    auth::{Auth, AuthMode},
    index::AppState,
};

#[derive(Parser, Debug)]
#[command(
    name = "gcs-pypi",
    version,
    about = "Serve a GCS bucket as a local Python package index for pip/uv",
    after_help = "With a command after `--`, the server starts on a free port, PIP_INDEX_URL and \
                  UV_DEFAULT_INDEX are set for the command, and the server stops when it exits.\n\
                  Without a command, the server runs until interrupted, or in the background with \
                  --daemon."
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

    /// Auth source
    #[arg(long, env = "GCS_PYPI_AUTH", value_enum, default_value = "auto")]
    auth: AuthMode,

    /// GCS API endpoint (override for emulators)
    #[arg(
        long,
        env = "GCS_PYPI_ENDPOINT",
        default_value = "https://storage.googleapis.com"
    )]
    endpoint: String,

    /// Log each request to stderr
    #[arg(long, short)]
    verbose: bool,

    /// Serve in the background: return once the server is ready, printing its env lines and
    /// GCS_PYPI_PID (stop it with `kill $GCS_PYPI_PID`)
    #[arg(long, conflicts_with = "command")]
    daemon: bool,

    /// Where the background server logs (with --daemon) [default: <temp dir>/gcs-pypi.log]
    #[arg(long, env = "GCS_PYPI_LOG_FILE")]
    log_file: Option<PathBuf>,

    /// Command to run with the index configured
    #[arg(last = true)]
    command: Vec<String>,
}

fn parse_bucket(spec: &str) -> Result<(String, String)> {
    let rest = spec.strip_prefix("gs://").unwrap_or(spec).trim_matches('/');
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    if bucket.is_empty() {
        bail!("invalid --bucket {spec:?}, expected gs://bucket[/prefix]");
    }
    let prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    Ok((bucket.to_string(), prefix))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (bucket, prefix) = parse_bucket(&args.bucket)?;
    if args.daemon && !daemon::is_child() {
        let log_file = args
            .log_file
            .unwrap_or_else(|| std::env::temp_dir().join("gcs-pypi.log"));
        return daemon::spawn(&log_file);
    }
    let upstream = match args.upstream.as_str() {
        "" | "none" => None,
        u => Some(if u.ends_with('/') {
            u.to_string()
        } else {
            format!("{u}/")
        }),
    };

    let state = Arc::new(AppState {
        http: reqwest::Client::builder().build()?,
        auth: Auth::new(args.auth)?,
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

    let app = server::router(state.clone());

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
    use clap::CommandFactory;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("gs://b", "b", "")]
    #[case("gs://b/", "b", "")]
    #[case("gs://b/a/c/", "b", "a/c/")]
    #[case("gs://b/a/c", "b", "a/c/")]
    #[case("b/pkgs", "b", "pkgs/")]
    fn parse_bucket_ok(#[case] spec: &str, #[case] bucket: &str, #[case] prefix: &str) {
        assert_eq!(parse_bucket(spec).unwrap(), (bucket.into(), prefix.into()));
    }

    #[rstest]
    #[case("")]
    #[case("gs://")]
    #[case("gs:///")]
    fn parse_bucket_err(#[case] spec: &str) {
        assert!(parse_bucket(spec).is_err());
    }

    #[test]
    fn args_definition_is_valid() {
        Args::command().debug_assert();
    }

    #[rstest]
    #[case(&[], AuthMode::Auto)]
    #[case(&["--auth", "metadata"], AuthMode::Metadata)]
    #[case(&["--auth", "gcloud"], AuthMode::Gcloud)]
    #[case(&["--auth", "none"], AuthMode::None)]
    fn auth_flag(#[case] extra: &[&str], #[case] expected: AuthMode) {
        let argv = [&["gcs-pypi", "--bucket", "gs://b"], extra].concat();
        assert_eq!(Args::try_parse_from(argv).unwrap().auth, expected);
    }

    #[test]
    fn daemon_conflicts_with_command() {
        assert!(Args::try_parse_from(["gcs-pypi", "--bucket", "gs://b", "--daemon", "--", "pip"]).is_err());
    }

    #[test]
    fn auth_flag_rejects_unknown() {
        assert!(Args::try_parse_from(["gcs-pypi", "--bucket", "gs://b", "--auth", "bogus"]).is_err());
    }
}
