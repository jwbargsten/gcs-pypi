# gcs-pypi

A ~3 MB static Rust binary that turns a GCS bucket of wheels/sdists into a local
PEP 503 index for **pip** and **uv**. It is meant to start right before an install
and stop right after.

```bash
gcs-pypi --bucket gs://my-pypi/packages -- pip install my-private-lib
gcs-pypi --bucket gs://my-pypi/packages -- uv sync
```

## How it works

```
pip / uv ──HTTP──▶ 127.0.0.1:<random>/simple/  (gcs-pypi)
                     │  project in bucket?  ── yes ─▶ HTML page generated from the bucket listing
                     │                       ── no ──▶ 307 redirect to https://pypi.org/simple/<name>/
                     └─ /files/<object> ─────────────▶ streamed from GCS with the VM's token
```

- **No index files in the bucket.** Drop `.whl` / `.tar.gz` / `.zip` files anywhere under
  the prefix. The listing is the index, so nothing needs regenerating and nothing goes stale.
- **Auth comes from the VM.** The token is fetched from the metadata server, which works on
  Vertex Workbench, GCE, GKE and Cloud Run. It is cached and refreshed before it expires.
  `--auth auto` falls back to `gcloud auth print-access-token` for laptops.
- **No dependency confusion.** A project that exists in the bucket is served *only* from
  the bucket and is never looked up upstream. Unknown projects are redirected to
  `--upstream`, which defaults to PyPI. Use `--upstream none` for air-gapped VMs, or set it
  to your mirror.
- **Hashes.** Every link carries `#md5=` from GCS metadata, so pip and uv verify each
  download. If an object has `x-goog-meta-sha256` metadata, that hash is used instead.
  `x-goog-meta-requires_python` becomes `data-requires-python`.
- **Fails fast.** The bucket is listed before the wrapped command starts. Missing
  permissions show up as one clear error rather than as a pip "no matching distribution".

## Usage

| Mode | Command |
|---|---|
| Wrap a command (recommended) | `gcs-pypi --bucket gs://b/prefix -- <cmd...>` sets `PIP_INDEX_URL` and `UV_DEFAULT_INDEX` for `<cmd>`, then exits with its exit code |
| Long-running | `gcs-pypi --bucket gs://b/prefix --addr 127.0.0.1:8765` prints the env lines and serves until Ctrl-C |
| Background | `eval "$(gcs-pypi --bucket gs://b/prefix --daemon)"` returns once the server is ready and sets the env vars plus `GCS_PYPI_PID` in your shell; stop it with `kill $GCS_PYPI_PID` |
| systemd service | see [Running as a systemd service](#running-as-a-systemd-service) |

| Flag / env | Default | |
|---|---|---|
| `--bucket` / `GCS_PYPI_BUCKET` | — | `gs://bucket[/prefix]` |
| `--upstream` / `GCS_PYPI_UPSTREAM` | `https://pypi.org/simple/` | `none` disables |
| `--auth` / `GCS_PYPI_AUTH` | `auto` | `metadata`, `gcloud`, `none` |
| `--cache-ttl` / `GCS_PYPI_CACHE_TTL` | `60` | seconds the listing is cached |
| `--addr` / `GCS_PYPI_ADDR` | `127.0.0.1:0` | port 0 = random free port |
| `--endpoint` / `GCS_PYPI_ENDPOINT` | `https://storage.googleapis.com` | for emulators |
| `-v` | off | log fetches and upstream redirects |
| `--daemon` | off | serve in the background (not with a command) |
| `--log-file` / `GCS_PYPI_LOG_FILE` | `<temp dir>/gcs-pypi.log` | log of the background server; if it fails to start, the error is also printed |

## Publishing

```bash
python -m build    # or: uv build
for f in dist/*; do
  gcloud storage cp "$f" gs://my-pypi/packages/ \
    --custom-metadata=sha256=$(sha256sum "$f" | cut -d' ' -f1) \
    --if-generation-match=0      # refuse to overwrite an existing version
done
```

The `sha256` metadata is optional; md5 works without it.

## Setup on Vertex Workbench / GCE

1. Grant the VM's service account `roles/storage.objectViewer` on the bucket.
2. Make sure the VM has the `cloud-platform` scope. Workbench instances have it by default.
3. Ship the binary in your image, or `gcloud storage cp` it from a bucket in a startup
   script. Build it once with `make dist`, which produces a fully static binary (see
   [Development](#development)).

Example Dockerfile or startup step:

```dockerfile
RUN gcs-pypi --bucket gs://my-pypi/packages -- pip install --no-cache-dir -r requirements.txt
```

## Running as a systemd service

`systemd/gcs-pypi.service` runs the server in the foreground on the fixed address
`127.0.0.1:8765`, with metadata-server auth and a dynamic, unprivileged user:

```bash
sudo install -m 0755 gcs-pypi /usr/local/bin/gcs-pypi
sudo install -m 0644 systemd/gcs-pypi.service /etc/systemd/system/
echo 'GCS_PYPI_BUCKET=gs://my-pypi/packages' | sudo tee /etc/default/gcs-pypi
sudo systemctl daemon-reload
sudo systemctl enable --now gcs-pypi
journalctl -u gcs-pypi -f
```

Then point pip and uv at it, e.g. in `/etc/pip.conf` and `/etc/uv/uv.toml`:

```ini
[global]
index-url = http://127.0.0.1:8765/simple/
```

```toml
[[index]]
url = "http://127.0.0.1:8765/simple/"
default = true
```

## Testing locally without GCP

`--auth none --endpoint http://localhost:4443` works against
[fake-gcs-server](https://github.com/fsouza/fake-gcs-server).

## Development

`make` lists the targets:

| Target | |
|---|---|
| `make build` | debug build |
| `make test` | run the unit tests |
| `make fmt` | apply `clippy --fix`, then `cargo fmt` |
| `make lint` | `cargo fmt --check` and `cargo clippy -D warnings`; fails on any warning |
| `make dist` | `clean`, `lint`, `test`, then an optimized build copied to `dist/gcs-pypi-<commit>-<target>` |
| `make clean` | `cargo clean` and remove `dist/` |

`make dist` builds for `x86_64-unknown-linux-musl` by default. That needs a Linux host with
the target and the musl toolchain:

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt-get install musl-tools
```

To build for another target, e.g. the host on macOS: `make dist DIST_TARGET=aarch64-apple-darwin`.

The toolchain (stable, with rustfmt and clippy) is pinned in `rust-toolchain.toml`. To skip the
reformat commit in `git blame`, run `git config blame.ignoreRevsFile .git-blame-ignore-revs`.
