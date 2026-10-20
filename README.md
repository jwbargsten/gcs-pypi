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

| Flag / env | Default | |
|---|---|---|
| `--bucket` / `GCS_PYPI_BUCKET` | — | `gs://bucket[/prefix]` |
| `--upstream` / `GCS_PYPI_UPSTREAM` | `https://pypi.org/simple/` | `none` disables |
| `--auth` / `GCS_PYPI_AUTH` | `auto` | `metadata`, `gcloud`, `none` |
| `--cache-ttl` / `GCS_PYPI_CACHE_TTL` | `60` | seconds the listing is cached |
| `--addr` / `GCS_PYPI_ADDR` | `127.0.0.1:0` | port 0 = random free port |
| `--endpoint` / `GCS_PYPI_ENDPOINT` | `https://storage.googleapis.com` | for emulators |
| `-v` | off | log fetches and upstream redirects |

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
   script. Build it once with `cargo build --release`; for a fully static binary, build
   with `--target x86_64-unknown-linux-musl`.

Example Dockerfile or startup step:

```dockerfile
RUN gcs-pypi --bucket gs://my-pypi/packages -- pip install --no-cache-dir -r requirements.txt
```

## Testing locally without GCP

`--auth none --endpoint http://localhost:4443` works against
[fake-gcs-server](https://github.com/fsouza/fake-gcs-server).
