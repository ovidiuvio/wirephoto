# wirephoto

Serves the photos on a machine's own disks to apps over HTTP: fast enough for browser apps, read and write, and
independent of any app. Put it on a NAS and a browser editor like lith can
browse, open, organise and save photos there.

- **Fast in a browser**: listings carry each photo's displayed size; previews come from the camera's embedded
  JPEG or a reduced-size decode, are cached on the server and forever in the browser, and hundreds stream in one
  request.
- **Every file type**: any file can be listed, downloaded, written, moved and trashed; previews for JPEG, PNG,
  GIF, WebP, TIFF, BMP, HEIC/HEIF/AVIF, camera RAW and video (a poster frame, with ffmpeg), decoded in sandboxed
  worker processes. HEIC, large JPEGs and video posters decode on the GPU (Intel Quick Sync and other VA-API
  GPUs) when one is mapped into the container.
- **Safe with your files**: the folders on disk stay the truth; byte writes are version-checked and
  integrity-checked; deletes go to a trash.
- **Small to run**: one container, one config file, one data folder.

[docs/design.md](docs/design.md) explains what it does and why; [docs/api.md](docs/api.md) is the HTTP contract.

## Run it

```bash
docker compose up -d
```

with [docker-compose.yml](docker-compose.yml) and a `wirephoto.toml` next to it (start from
[wirephoto.example.toml](wirephoto.example.toml)). The image is published for `linux/amd64` and `linux/arm64` at
`ghcr.io/ovidiuvio/wirephoto`.

On a Synology NAS, follow [docs/synology.md](docs/synology.md): Container Manager, permissions, decoding on the
GPU (Intel Quick Sync), watch limits and HTTPS, step by step.

Create a key for each app, on the server:

```bash
docker compose exec wirephoto wirephoto keys create --name lith --scope write --origin https://lith.example.com
```

The key is printed once. `wirephoto keys list` and `wirephoto keys revoke <name>` manage them.

Keeping an eye on it (all run where the server runs, e.g. `docker compose exec wirephoto wirephoto status`):

| Command | |
|---|---|
| `wirephoto status` | Each library's progress, speed and time left; how long previews take by file type; cache and workers |
| `wirephoto audit [--since 7d] [--key NAME] [--op OP]` | The audit trail: uploads, moves, deletes, restores, refused requests, key changes |
| `wirephoto pause` / `resume` | Hold background work (reading files, previews, hashing), also across restarts |
| `wirephoto cache stats\|list\|extract DIR` | Look inside the preview cache |

The log (`docker logs`, and a file a day in `<data>/logs`) has a progress line every 30 seconds while there is
work, and the audit trail; one line per API request goes to its own file, `<data>/logs/access.log.<date>`. Background work never holds up requests: it runs at low CPU
priority, and while the API is in use previews go one at a time and hashing waits.

Browser apps on `https://` can only reach an `https://` server: give wirephoto a certificate (`[tls]` in the
config; `tailscale cert` makes one for a tailnet name) or put a TLS-terminating reverse proxy in front.

## Try the API

```bash
KEY=wp_...
curl -s -H "Authorization: Bearer $KEY" https://nas:8443/v1/libraries
curl -s -H "Authorization: Bearer $KEY" "https://nas:8443/v1/entries?library=lib_photos&kind=folder"
```

Every file entry carries ready-made `urls.content` and `urls.preview` (replace `{size}` with 160, 320, 480, 640,
1280 or 2560); they are signed for the key and work directly in `<img>` and `<video>`.

## Build from source

Needs Rust (stable), a C++17 compiler and cmake (jpegli is built from `crates/jpegli-sys/vendor`; turn it off
with `--no-default-features --features heic` to encode the cached previews with libjpeg-turbo instead),
libheif ≥ 1.17 and libjpeg-turbo ≥ 3 (or build it too: `TURBOJPEG_SOURCE=vendor`).

```bash
brew install cmake libheif jpeg-turbo      # macOS
cargo test --workspace
cargo run --release -- -c wirephoto.toml serve
```

## Status

The API in docs/api.md is implemented and tested end to end (`crates/wirephoto/tests/api.rs`). Not done yet,
from the design:

- PSD/PSB, JPEG XL, HDR tone-mapping, LibRaw for RAWs without an embedded preview
- the decode workers' seccomp filter is a deny list (network, file creation and changes, io_uring, kernel and
  process interfaces), installed with `seccomp()` or, on older kernels (many NAS systems), `prctl`; a kernel with
  no seccomp filters at all gets none (the worker says so on stderr; `WIREPHOTO_SECCOMP=required` refuses
  instead). Running as root, workers drop to `nobody`; otherwise they share the server's user and could still
  signal it; with a GPU in use they may also open files read-write (its render node), still not create or truncate them
- GPU decoding is tested with VideoToolbox on macOS and the CPU path everywhere; VA-API (Quick Sync) on real
  hardware is checked by the startup probe, whose result `wirephoto status` shows
- zstd/brotli for downloads of uncompressed RAW and TIFF; multi-range requests
- neighbour readahead when an original is opened; newest-first preview scheduling
- a generated OpenAPI document
- a library whose storage disappears at runtime (an unmounted share) answering `503` instead of errors

## License

MIT
