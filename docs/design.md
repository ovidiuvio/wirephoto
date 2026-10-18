# wirephoto — design

wirephoto serves the photos on a machine's own disks to apps over HTTP. It indexes folders where they already
are (NAS volumes, local disks, shares mounted on the host), serves originals and previews fast enough for
browser apps, and lets apps write files back. It knows nothing about any particular app: it serves files,
folders and the facts needed to serve them fast, and nothing else.

The HTTP contract is [api.md](api.md). This document covers what wirephoto does, how it is built, and why.

## Goals

1. **Fast in a browser.** A grid of a 10,000-photo folder paints at once and scrolls without waiting. A RAW opens
   as fast as the network allows.
2. **Simple.** Four concepts, one way to do each thing, standards where they exist.
3. **App-independent.** Any app can use it; no app's conventions are built in.
4. **Safe with your files.** Files change only when an app deliberately writes them, every byte write is
   version-checked and integrity-checked, and deletes go to a trash first.
5. **Small to run.** One container, one config file, one data folder. No database server, no extra services.

## Scope

wirephoto serves **storage it can open as a local path**: disks in the machine, and network shares (NFS, SMB)
mounted on the host and passed into the container. It does not talk to cloud storage or remote servers.

It reads from each photo only what serving needs: its displayed size and orientation (for layout and correct
previews), capture time (for sorting), duration (videos) and a SHA-256 hash.

## Not wirephoto's job

| | Whose job |
|---|---|
| Ratings, labels, keywords, titles, edits, albums, search by camera or lens | The apps. They keep it in their own storage, or write it into the library as ordinary files (to wirephoto, an `.xmp` is just a file). |
| Grouping RAW + JPEG pairs or Live Photos | The apps, from the listing (same folder, same base name). |
| Organising the library | The person, through apps. wirephoto never moves, renames, imports or merges files on its own. The folders on disk are the truth; wirephoto's index is a cache of them. |
| Arbitrary resizing | Not offered: previews come in a fixed set of sizes. |
| RAW development | Apps that develop RAWs (lith) read the original. |
| Backups, bit-rot repair, storage deduplication | The filesystem and backup tools (ZFS, Btrfs). |

## Concepts

| Concept | What it is |
|---|---|
| **Library** | A folder wirephoto serves, set in the config file. |
| **Entry** | A folder or a file in a library. Everything an app reads or changes is an entry. |
| **Key** | An app's access: a secret with a scope, optionally tied to a browser origin. Created on the server. |
| **Change** | A numbered record of what happened to an entry, for catching up or listening live. |

## Feature set

### v1

**Libraries**
- Any folder the server can open, read-only or read-write.
- Changes made outside wirephoto (Finder, Lightroom, a camera import) appear live (inotify), or by polling on
  network mounts and wherever the watch limit is too low. Each library reports which.
- A million files and more: the first scan lists folders first and fills in media facts and hashes in the
  background, reporting progress; the library is usable throughout.

**Files**
- Any file type can be listed, downloaded, written, copied, moved and trashed. Kinds are decided from content.
- Previews for JPEG, PNG, GIF, WebP, AVIF, HEIC/HEIF, JPEG XL, TIFF, BMP, PSD/PSB, camera RAW (from the
  embedded preview; LibRaw when there is none) and video (a poster frame).

**Reading**
- Listings by folder (optionally recursive) or library, filtered by kind and capture date, sorted by name,
  capture time, modification time or size, paged.
- Every photo's displayed size and capture time in the listing.
- Originals with range requests, verification digests, and compression for uncompressed RAW and TIFF.
- Previews in six sizes (160–2560 px), all JPEG, cached by the browser until the file changes; hundreds
  per request, streamed, visible first, cancelled when scrolled past. The 2560 px size is capped at a RAW's
  embedded preview unless the library is set to decode RAWs fully.

**Writing**
- New files, replacing files, folders, rename, move, copy (across libraries; instant where the filesystem shares
  blocks), trash and restore, batches of up to 100 requests.
- Every byte write is conditional and can be integrity-checked; files appear whole or not at all.
- Fast uploads: parallel over HTTP/2, streamed straight to disk, hash and previews made while the bytes arrive.

**Deduplication**
- A SHA-256 for every file: while it is written, on demand, and in the background.
- Lookups by hash (skip an upload the server already has, or copy on the server instead), and listing of
  duplicates.

**Changes**
- Catch up from a sequence number, or stay connected for live changes.

**Security and operations**
- TLS; API keys with scopes, library limits and origin binding; signed URLs for `<img>` and `<video>`.
- One image for amd64 and arm64; `wirephoto keys` on the server; `Server-Timing` on byte responses.

### Later

| | |
|---|---|
| Resumable uploads (tus) | Single-request uploads are enough on a LAN |
| Video transcoding (HLS) | For codecs browsers cannot play; large dependency and CPU cost |
| `wirephoto verify` | Re-hash files against stored hashes to detect bit rot |
| Sign-in with OAuth | A second way to get the same bearer tokens, for multiple users or third-party apps |
| Contact-sheet atlases, HTTP/3 | Optimisations, to measure against v1 first |
| A smaller preview format for slow links (WebP/AVIF) | Only pays off away from the LAN; additive when needed |

### Left out on purpose

| Left out | Instead |
|---|---|
| Cloud and remote storage | Mount it on the host if it can be mounted. |
| App data, sidecar handling, stacks | Apps own their data and conventions. |
| Prefetch / viewport hints | Preview order sets priority, aborting cancels, opening an original warms its neighbours. |
| Long-running operations | Every request finishes on its own. |
| A trash resource | Entries have `trashed`; restoring sets it back. |
| Per-resource batch methods | One `/v1/batch` runs ordinary requests. |
| Arbitrary preview sizes, other formats (WebP, AVIF), colour options | A fixed ladder in one format keeps the cache small and URLs cacheable; JPEG decodes fastest. Colour-critical work uses the original. |
| Key management and admin over HTTP | The `wirephoto` command on the server and the config file. |

## Architecture

```
                HTTPS (HTTP/2)
apps ───────────────────────────► http (axum)
                                    │  keys · CORS · signed URLs · routes
                 ┌──────────────────┼──────────────────────┐
                 ▼                  ▼                      ▼
              index             previews                 files
           (SQLite: entries,   (scheduler → memory      (range reads,
            hashes, changes,    → disk cache →           compression,
            keys)               decode workers)          atomic writes,
                 ▲                  │                    digests, trash)
                 │                  │                      │
              indexer ◄─────────────┴──────────────────────┘
           (scan · watch · poll · hash)        │
                                               ▼
                                     library folders (/photos, …)
```

**One binary.** `wirephoto serve` runs the server and a pool of `wirephoto decode` worker processes for pixel
work. `wirephoto keys …` manages keys. Nothing else runs.

| Module | Responsibility |
|---|---|
| `files` | All disk access, confined to the library roots by the kernel (`openat2` with `RESOLVE_BENEATH`, via `cap-std`). Range reads, streamed atomic writes (temp file in the target folder, fsync, rename), digest checks, copies (reflinks where available), the trash folder. |
| `indexer` | Keeps the index equal to the disk. Start-up scan in passes: folders and names first, then media facts (EXIF orientation, capture time, dimensions), then hashes, each at idle priority and in inode order. Then inotify, or polling of folder modification times (only changed folders are re-read). Keeps a file's ID across renames and moves. |
| `index` | SQLite (WAL): entries, hashes, the change log (30 days), keys. Deleting it costs a rescan, never data. |
| `previews` | Memory cache (moka), then disk cache (redb), then the scheduler: identical requests coalesce, newest first, work nobody waits for is cancelled. Pre-generates the 640 px preview of every photo, the size filmstrips and galleries use. |
| `decode` | Worker processes: RAW embedded previews and LibRaw, libjpeg-turbo with DCT scaling, libheif, libvips for other formats, ffmpeg for video posters; resize with `fast_image_resize`. Encodes with **jpegli** (4:2:0, baseline) for the cached 640 px previews, where size matters, and **libjpeg-turbo** for the sizes made on demand, where speed matters (see Previews). |
| `http` | axum, hyper, rustls. Key checks, CORS, URL signing, problem+json errors, the generated OpenAPI document. |

**Writes go through one pipeline.** Bytes arriving in a `PUT` are streamed to the temp file and, on the way,
hashed (and checked against `Content-Digest`), parsed for media facts, and fed to the decoder for the 640 px
preview. When the rename makes the file visible, its entry is complete: an uploaded photo is in every app's grid
at once, and the server never reads it back from disk.

## Previews

Every preview is a baseline JPEG: sRGB, 8-bit, orientation applied, metadata stripped. One format keeps the
cache, the URLs and the apps simple, and JPEG is the fastest format for browsers to decode. Any file the server
can decode gets one; what JPEG cannot hold is resolved by fixed rules (api.md, section 5): transparency onto
neutral grey, the first frame or page, HDR tone-mapped, everything converted to 8-bit sRGB.

| Size | Made | Encoder | Why |
|---|---|---|---|
| **640 px** | Ahead of time for every photo (and on demand until then), kept in the cache | **jpegli**, 4:2:0, baseline | The size filmstrips and galleries use: sharp in 3:2 cells up to ~210 CSS px tall at 2x. Stored for every photo, so size matters most |
| 160, 320, 480 px | On demand **from the cached 640 px preview** (rendering it first if missing), cached, evicted first | **libjpeg-turbo** | ~1 ms instead of decoding the original again (40 MP: 20–470 ms) |
| 1280, 2560 px | On demand from the original, cached, evicted first | **libjpeg-turbo** | Viewed one at a time; latency matters most |

Measured on lith's test corpus (16 camera and phone images, 640 px, `scripts/bench-jpeg.py`), at equal
SSIMULACRA2 quality:

| Encoder | Size vs libjpeg-turbo (score 70 / 80 / 85) | Encode, 640 px |
|---|---|---|
| libjpeg-turbo | — | 1.9 ms |
| **jpegli 4:2:0 baseline** | **−14% / −16% / −13%** | **5.8 ms** |
| mozjpeg baseline | −12% / −2% / +6% | 13.4 ms |

jpegli is about 15% smaller at every quality that matters and faster than mozjpeg; progressive mode would save
only another ~1% while decoding slower. jpegli has no mature Rust crate, so the decode workers link its static
library through its libjpeg-compatible C interface, alongside the other C libraries.

The cache targets SSIMULACRA2 ≈ 70 ("high quality"): jpegli quality 68, where a 640 px preview averages ~31 KB
(libjpeg-turbo quality 80, used before, scored ~78 at ~50 KB). `crates/jpegli-sys` vendors jpegli and Highway
(`scripts/vendor-jpegli.sh` refreshes them) and builds them with cmake behind a one-call RGB-to-JPEG shim.

**Crates**: `wirephoto` (the binary with the modules above) and `wirephoto-api` (request and response types, the
OpenAPI document, shared with generated clients).

## Performance

How the server behaves; none of it adds API.

| Technique | Effect |
|---|---|
| Displayed size in every listing | The grid lays out before any image loads. |
| Fixed preview sizes, JPEG only, orientation applied, sRGB | The browser decodes only the pixels it shows, with its fastest decoder. |
| RAW embedded previews; JPEG DCT scaling (decode at 1/2, 1/4, 1/8); built-in HEIC/EXIF thumbnails for small sizes | Most previews never decode the full image. |
| Preview URLs versioned by `etag`, cached as immutable | Revisiting a grid makes no requests. |
| Streamed batch previews: priority by order, completion order, cancel on abort | One request and one preflight for a screenful; nothing is made for cells scrolled past. |
| Newest-first scheduling, coalescing | What is on screen is made first, and once. |
| Preview cache in redb on the data volume, memory cache above it | Millions of small previews without millions of small files; browsing does not spin up the photo disks. |
| Background indexing and generation in inode order, idle priority | Hard disks read sequentially instead of seeking. |
| Neighbour readahead on opening an original | The next and previous files are already in memory. |
| Hash, media facts and previews made from upload bytes in flight | New files are complete on arrival without being read back. |
| Hash lookups narrowed by size first | A dedup lookup rarely hashes more than one file on demand. |
| zstd/brotli for full downloads of uncompressed RAW and TIFF | 30–50% fewer bytes for those files. |
| HTTP/2 with large flow-control windows | One big transfer fills the link; parallel uploads share one connection. |
| `Server-Timing` on byte responses | Cache hits, read, decode and encode times show in the browser's devtools. |

**Sizing for a million photos**: a 640 px preview is about 35 KB (more for detailed or noisy photos), so the
pre-generated size takes about 35 GB per million photos; other sizes are made on demand and evicted first. Set
`cache.max_size` accordingly (less means older previews are regenerated on demand). The first full
hash of 30 TB reads for about 40 hours at idle priority. inotify needs one watch per folder; when the host's
limit (`fs.inotify.max_user_watches`) is lower than the folder count, the library switches to polling and says
so.

## Security

- **TLS**: certificate files (from ACME or `tailscale cert`), or a reverse proxy in front that terminates TLS.
  A browser app on `https://` cannot reach a plain-HTTP server.
- **Keys**: 32 random bytes, shown once, stored as SHA-256 hashes. Scope `read` or `write`, optionally per
  library. A key tied to an origin is refused from any other browser origin, and CORS allows only key origins.
  `wirephoto keys list` shows when each key was last used.
- **Signed URLs**: bound to the key that received them; revoking the key revokes them.
- **Confinement**: every path is resolved beneath its library root by the kernel; `..` and escaping symlinks
  fail by construction.
- **Decoders**: image and video decoders are the largest attack surface (any written file reaches them), so pixel
  work runs in worker processes with seccomp, no network and a read-only file descriptor. A crash costs one
  preview.
- **Container**: non-root (`PUID`/`PGID`), read-only root filesystem, no capabilities, distroless base. A
  read-only library can be mounted `:ro`.
- **Writes**: conditional, integrity-checkable, logged with the key that made them; deletes go through the
  trash.

## Configuration

```toml
public_url = "https://photos.home.example.com"   # used in signed URLs
listen = "0.0.0.0:8443"

[tls]                    # omit behind a reverse proxy that terminates TLS
cert = "/data/tls/cert.pem"
key = "/data/tls/key.pem"

[[library]]
id = "nas"               # lib_nas
name = "NAS Photos"
path = "/photos"
read_only = false
trash_days = 30          # 0: deletes are permanent
watch = "auto"           # auto | poll
poll_seconds = 60
raw_full_decode = false  # true: 2560 px previews decode the RAW when its embedded preview is smaller

[cache]
max_size = "50GB"        # previews on disk, all sizes; least recently used go first, 640 px last
memory = "256MB"         # previews kept in memory in front of the disk cache

[decode]
workers = 0              # previews rendered at once; 0: one per CPU
ffmpeg = "/usr/bin/ffmpeg"  # video posters and durations; default: from PATH, none without it
```

`/data` holds the index, the preview cache and TLS files. Put it on an SSD.

## Deployment

```yaml
services:
  wirephoto:
    image: ghcr.io/ovidiuvio/wirephoto:1
    user: "1026:100"
    read_only: true
    cap_drop: [ALL]
    ports: ["8443:8443"]
    volumes:
      - ./wirephoto.toml:/etc/wirephoto.toml:ro
      - /volume1/photos:/photos
      - /volume1/docker/wirephoto:/data
    restart: unless-stopped
```
