# wirephoto HTTP API — v1

The contract between wirephoto and apps. The OpenAPI document is generated from the server code and must
match this file; when they disagree, this file is the intent and the code is the bug. Concepts and rationale
are in [design.md](design.md).

## Contents

1. [Conventions](#1-conventions)
2. [Authentication](#2-authentication)
3. [Server and libraries](#3-server-and-libraries)
4. [Entries](#4-entries)
5. [Reading bytes](#5-reading-bytes)
6. [Writing](#6-writing)
7. [Batch](#7-batch)
8. [Changes](#8-changes)
9. [Errors](#9-errors)
10. [Endpoint index](#10-endpoint-index)

## 1. Conventions

1. **Resources and HTTP methods.** `GET` reads, `POST` creates from JSON, `PUT` writes bytes, `PATCH` changes
   fields, `DELETE` deletes. Paths are nouns.
2. **JSON** for everything except file bytes and previews. Fields are `camelCase`, timestamps RFC 3339, sizes in
   bytes, durations in seconds. A field that does not apply is omitted.
3. **IDs** are opaque strings: `lib_…` for libraries, `ent_…` for entries. An entry keeps its ID when it is
   renamed, moved or trashed. Clients never build IDs from paths.
4. **URLs are given, not built.** Entries carry the URLs of their bytes and previews; clients use them as they
   are.
5. **Versions.** Every entry has an `etag`, also sent as the `ETag` header. `PATCH` and `DELETE` honour
   `If-Match` when sent.
6. **Nothing is overwritten or permanently deleted.** Writing bytes always makes a new file, and identical
   bytes are not stored twice (section 6). A name that is taken never fails a request: the server uses the next
   free one (`IMG_1.jpg`, `IMG_1-1.jpg`, `IMG_1-2.jpg`…) and the response says which. Deletes go to the trash.
7. **Digests.** File hashes are SHA-256, written as lowercase hex in JSON and in the RFC 9530 form in headers
   (`sha-256=:<base64>:`).
8. **Lists** are paged with `limit` (default 200, maximum 1000) and an opaque `cursor`. The response is
   `{"items": [...], "next": "<cursor>"}`; `next` is omitted on the last page.
9. **Errors** are `application/problem+json` (RFC 9457) with a stable `code` (section 9).
10. **Compatibility.** New fields, endpoints and enum values are added without changing `v1`; clients ignore what
   they do not know.

## 2. Authentication

Every request except `GET /v1` and `GET /healthz` carries an API key:

```
Authorization: Bearer wp_k3JxQ0r7…
```

Keys are created, listed and revoked on the server, never through the API:

```bash
wirephoto keys create --name laptop-browser --scope write --origin https://lith.example.com
wirephoto keys list
wirephoto keys revoke laptop-browser
```

- **Scope** is `read` or `write` (which includes deleting), optionally limited to some libraries
  (`--library nas`).
- **Origin.** A key created with `--origin` can be used from that browser origin and no other: a browser request
  carrying the key from another origin is rejected. Requests without an `Origin` header (curl, scripts) are not
  affected. A key without `--origin` is for scripts only.
- **CORS** allows exactly the origins of existing keys. Preflights are cached for two hours, and Private/Local
  Network Access preflights are answered for the same origins. Responses carry
  `Cross-Origin-Resource-Policy: cross-origin`, so a cross-origin isolated app (`COEP: require-corp`) can show
  previews in `<img>`; the signature guards them, not the embedding origin.
- **Signed URLs.** The byte and preview URLs in an entry carry a `sig` bound to the key that received them, so
  they work in `<img>` and `<video>` without a header. A `sig` stays the same for the life of the key (the
  browser can cache by URL) and stops working when the key is revoked.

## 3. Server and libraries

### `GET /v1`

```json
{
  "name": "wirephoto",
  "version": "1.0.0",
  "previews": {"sizes": [160, 320, 480, 640, 1280, 2560]},
  "limits": {"pageSize": 1000, "batch": 100, "previews": 500},
  "key": {"name": "laptop-browser", "scope": "write", "libraries": ["lib_nas"]}
}
```

No key required. `key` describes the key sent with the request, and is omitted when there is none.

### Libraries

A library is a folder on the server's disks (a mounted share counts), defined in the config file.

```json
{
  "id": "lib_nas",
  "name": "NAS Photos",
  "root": "ent_0a1b",
  "readOnly": false,
  "trashDays": 30,
  "indexing": {"state": "scanning", "done": 412000, "total": 1180000},
  "watch": "live"
}
```

| | |
|---|---|
| `GET /v1/libraries` | The libraries the key can use. Not paged. |
| `GET /v1/libraries/{id}` | One library. |

| Field | |
|---|---|
| `root` | The library's top folder. |
| `trashDays` | How long a deleted entry stays in the trash before the server removes it; `0`: until removed by hand on the server. Nothing in the API deletes permanently. |
| `indexing` | Present while the server is still reading the library. Entries are listed from the start; `media` and `sha256` fill in as indexing proceeds. `state` is `scanning` (reading the folders), `reading` (media facts) or `hashing`. |
| `watch` | `live` (changes on disk appear at once) or `polling` (they appear within the poll interval, as on network mounts). |

## 4. Entries

An entry is a folder or a file of any type.

```json
{
  "id": "ent_k82Pq",
  "library": "lib_nas",
  "parent": "ent_7c2e",
  "kind": "raw",
  "name": "DSC01234.ARW",
  "path": "/2026/2026-10 Lisbon/DSC01234.ARW",
  "mime": "image/x-sony-arw",
  "size": 48213344,
  "modified": "2026-10-12T09:14:03Z",
  "etag": "3a9f0c1e",
  "sha256": "9f2c4e…",
  "media": {
    "width": 6000,
    "height": 4000,
    "orientation": 6,
    "taken": "2026-10-12T09:14:03+01:00"
  },
  "urls": {
    "content": "/v1/entries/ent_k82Pq/content?sig=k7.Hk2xQ",
    "preview": "/v1/entries/ent_k82Pq/preview/{size}.jpg?v=3a9f0c1e&sig=k7.Hk2xQ"
  }
}
```

| Field | |
|---|---|
| `kind` | `folder`, `image`, `raw`, `video` or `other`, decided from the file's content. |
| `parent` | Omitted for a library's root folder. |
| `path` | From the library's root. For display; address entries by `id`. |
| `mime`, `size` | Files only. |
| `etag` | Changes when a file's bytes change, or when a folder is renamed or moved. |
| `sha256` | The file's hash, once computed (section 6, Deduplication). |
| `media` | Photos and videos: what the server reads to serve them. `width` and `height` are the **displayed** size (orientation applied), so a grid can be laid out before any image loads; `orientation` is the EXIF value the previews already apply; `taken` is the capture time; `duration` (videos) in seconds. Nothing else: describing photos is the apps' business. |
| `trashed` | `true` while the entry is in the trash; omitted otherwise. |
| `urls.content` | The file's bytes. Files only. |
| `urls.preview` | A template: replace `{size}` with one of the sizes from `GET /v1`. Present only when the server can preview the file. |
| `counts` | Folders only: `{"folders": 2, "files": 412}`. |

### `GET /v1/entries/{id}`

One entry.

### `GET /v1/entries`

A list of entries. Exactly one scope:

| Scope | Lists |
|---|---|
| `parent={id}` | The folder's children. With `recursive=true`, everything below it. |
| `library={id}` | Everything in the library. With `kind=folder`, the whole folder tree. |
| `library={id}&path={path}` | The entry at that path, if any. |

| Option | |
|---|---|
| `kind` | Comma-separated kinds. |
| `takenAfter`, `takenBefore` | Capture time. |
| `sha256` | Files with this hash (section 6, Deduplication). |
| `size` | With `sha256` and `library`: files of this size not hashed yet are hashed first, so the lookup cannot miss them. |
| `duplicates=true` | Only files whose hash appears more than once, sorted by hash so copies are adjacent. |
| `trashed=true` | List the trash instead. |
| `sort` | `name` (default), `taken`, `modified`, `size`. Prefix `-` to reverse. |

## 5. Reading bytes

### `GET /v1/entries/{id}/content`

The file as stored.

- Supports `Range` and conditional requests (`If-None-Match`, `If-Range`).
- Sends `Repr-Digest` when the file's hash is known, so the client can verify what it received.
- A full download of a file that compresses well (uncompressed RAW, TIFF) may be sent with
  `Content-Encoding: zstd` or `br` when the client accepts it. Range responses are never compressed.

### `GET /v1/entries/{id}/preview/{size}.jpg`

A preview: a baseline JPEG no larger than `size` pixels on its long edge, sRGB, 8-bit, orientation applied, no
metadata.

- A preview is never larger than the original, nor than the camera's embedded preview for RAW files unless the
  library is set to decode RAWs fully.
- Every previewable file becomes the same kind of JPEG:

  | Source | Preview |
  |---|---|
  | RAW | The camera's embedded JPEG; a full decode when there is none |
  | Transparency (PNG, WebP, GIF, HEIC/AVIF, PSD) | Composited onto neutral grey |
  | Animation (GIF, WebP, APNG) | The first frame |
  | Several pages or layers (TIFF, PSD) | The first page, or the flattened image |
  | HDR (gain-map JPEG, HDR HEIC/AVIF, EXR) | Tone-mapped to standard range |
  | Wide gamut, CMYK, 16-bit or floating point | Converted to 8-bit sRGB |
  | Video | The poster frame |

  Previews are for seeing a file quickly; colour-critical and HDR work uses the original.
- With `v` equal to the file's current `etag`, it is cached for good
  (`Cache-Control: private, max-age=31536000, immutable`). With an old `v`, the response is `307` to the
  current URL.
- `415` if the file cannot be previewed.

### `POST /v1/previews`

Many previews in one streamed response.

```json
{"ids": ["ent_k82Pq", "ent_k82Pt", "..."], "size": 640}
```

- Up to `limits.previews` IDs. **Order is priority**: list the visible ones first.
- The response, `application/vnd.wirephoto.frames`, is a sequence of frames sent as each preview is ready
  (cached ones first):

  | Bytes | Field | |
  |---|---|---|
  | 4 | `index` | The ID's position in `ids` (unsigned, little-endian) |
  | 2 | `status` | `200`, or this item's error status (`404`, `415`, ...) |
  | 2 | — | Reserved, `0` |
  | 4 | `length` | Payload length (unsigned, little-endian) |
  | `length` | payload | The JPEG, or a problem+json document when `status` is not `200` |

- Aborting the request cancels the previews not yet sent.
- The browser does not cache these. Apps keep them in their own cache, keyed by entry ID and `etag`.

The byte and preview endpoints accept the key in the `Authorization` header or the `sig` of the given URL.

## 6. Writing

Writing needs a `write` key and a library that is not read-only.

### `PUT /v1/entries/{folder}/children/{name}` — new file

The request body is the file. Never refused for its name, and never overwrites anything:

- If the folder already holds a file with exactly these bytes (under any name), nothing is stored: `200` with
  that file's entry.
- Otherwise the file is stored under `name`, or under the next free name if `name` is taken (`name-1.jpg`, …):
  `201` with the new entry, whose `name` says which, and whose `sha256` and `media` are already filled in.

The file appears whole or not at all. `If-None-Match: *` is accepted and changes nothing.

### `PUT /v1/entries/{id}/content` — new contents for a file

Saved as a **new file beside it**; the original is never overwritten. The same as a `PUT` into its folder
under its name: identical bytes store nothing (`200` with the existing entry), different bytes become the next
free name (`201` with the new entry). `If-Match` is not needed.

### Integrity

Both `PUT`s accept `Content-Digest: sha-256=:<base64>:` (RFC 9530). The server checks the received bytes
against it and, on a mismatch, discards them and answers `400 digest-mismatch`; nothing is written.

### `POST /v1/entries` — new folder, or a copy

```json
{"parent": "ent_7c2e", "name": "Selects", "kind": "folder"}
```

```json
{"parent": "ent_9f00", "copyOf": "ent_k82Pq"}
```

- `kind: "folder"` creates a folder. `copyOf` copies a file, or a folder with everything in it; `name`
  defaults to the original's.
- Copies may go to another library. Where the filesystem supports it, a copy shares the original's blocks and is
  instant.
- A copy is a deliberate duplicate: it is made even when the bytes are already there. A taken name gets the
  next free one (`Selects-1`, `IMG_1-1.jpg`). `201` with the new entry.

### `PATCH /v1/entries/{id}` — rename, move, restore

The body is a JSON Merge Patch (RFC 7396):

```json
{"name": "Lisbon-001.ARW", "parent": "ent_9f00"}
```

| Field | |
|---|---|
| `name`, `parent` | Rename, move. Within one library. A taken name gets the next free one. |
| `trashed: false` | Restore from the trash to where it was, under its old name or, if that is taken now, the next free one. |

`200` with the updated entry.

### `DELETE /v1/entries/{id}`

Moves the entry (a folder with everything in it) to the library's trash; it keeps its ID and gets
`trashed: true`. `204`.

**Nothing in the API deletes permanently.** The server empties the trash after the library's `trashDays`
(never, when it is `0`); until then `PATCH {"trashed": false}` restores. Deleting an entry already in the trash
is `409`, and `?permanent=true` is `400`.

### Deduplication

Every file gets a `sha256`: computed while it is written through the API, on demand when a lookup needs it,
and in the background for files already in the library.

- **Before uploading**, hash the file and look it up: `GET /v1/entries?library={id}&sha256={hex}&size={bytes}`
  (up to 100 lookups per `/v1/batch`). A match means the server already has those bytes.
- **To have it in another folder anyway**, copy the match (`POST /v1/entries` with `copyOf`) instead of
  uploading it.
- **To find duplicates** already in a library: `GET /v1/entries?library={id}&duplicates=true`.

The server never merges or removes duplicates itself.

## 7. Batch

### `POST /v1/batch`

Runs up to `limits.batch` JSON requests in one: for acting on a selection without a round trip and a CORS
preflight per file.

```json
{
  "requests": [
    {"method": "PATCH", "path": "/v1/entries/ent_k82Pq", "body": {"parent": "ent_9f00"}},
    {"method": "DELETE", "path": "/v1/entries/ent_k82Pr", "headers": {"If-Match": "77c0e1aa"}},
    {"method": "GET", "path": "/v1/entries?library=lib_nas&sha256=9f2c4e…"}
  ]
}
```

```json
{
  "responses": [
    {"status": 200, "body": {"id": "ent_k82Pq", "...": "..."}},
    {"status": 412, "body": {"code": "version-mismatch", "...": "..."}},
    {"status": 200, "body": {"items": []}}
  ]
}
```

- Each request behaves exactly as if sent alone.
- They run in order, one at a time. A failure does not stop the rest; nothing is rolled back.
- Allowed: `GET`, `POST`, `PATCH` and `DELETE` on `/v1/entries…`, without bytes.

## 8. Changes

Every change to an entry gets an increasing sequence number. Apps keep their own copy of what they show and
catch up from the last number they saw.

```json
{"seq": 918273, "type": "updated", "entry": {"id": "ent_k82Pq", "...": "the entry after the change"}}
```

- `type` is `created`, `updated` or `deleted`. A `deleted` change's `entry` holds only `id`, `library` and
  `parent`. Trashing and restoring are `updated` (see `trashed`).
- Indexing filling in `media` or `sha256` is an `updated` change too.
- Each change carries the whole entry, so applying one twice is harmless.

### `GET /v1/changes`

| Option | |
|---|---|
| `since` | Changes after this number. Without it, the response only gives the current number. |
| `library` | One library's changes. |
| `limit` | Default 200, maximum 1000. |

Response: `{"changes": [...], "next": 918273}`. Pass `next` as `since` next time; fewer than `limit` changes
means the app is up to date.

**Live**: with `Accept: text/event-stream` the response stays open and sends each change as it happens
(`id:` is its `seq`). Reconnecting with `Last-Event-ID` resumes. Read it with `fetch()` to send the key.

**First sync**: get the current number, list the entries, then follow changes from that number.

`410` when `since` is older than the history the server keeps (30 days): list again.

## 9. Errors

```json
{
  "type": "https://wirephoto.dev/errors/version-mismatch",
  "title": "The entry changed since you read it",
  "status": 412,
  "code": "version-mismatch",
  "detail": "If-Match 3a9f0c1e, current 9b0e2f44"
}
```

Apps act on `code`; `title` and `detail` are for people.

| `code` | Status | When |
|---|---|---|
| `invalid-request` | 400 | Malformed body, bad parameter, no scope on a list |
| `digest-mismatch` | 400 | Uploaded bytes do not match `Content-Digest` |
| `unauthenticated` | 401 | Missing, unknown or revoked key; bad `sig` |
| `forbidden` | 403 | Read-only key or library; key used from another origin |
| `not-found` | 404 | No such entry or library |
| `conflict` | 409 | Move across libraries; deleting what is in the trash; a folder into itself |
| `history-expired` | 410 | `since` is older than the kept history |
| `version-mismatch` | 412 | `If-Match` failed (`PATCH`, `DELETE`) |
| `too-large` | 413 | Over a limit (batch, previews) |
| `unsupported-media` | 415 | Cannot preview this file |
| `preview-failed` | 503 | The preview could not be made this time (a decoder crashed, the file could not be opened); try again |
| `internal` | 500 | A bug |
| `storage-unavailable` | 503 | The library's folder cannot be read (an unmounted share) |

## 10. Endpoint index

| Method | Path | |
|---|---|---|
| GET | `/healthz` | Up |
| GET | `/v1` | Server, limits, current key |
| GET | `/v1/libraries`, `/v1/libraries/{id}` | Libraries |
| GET | `/v1/entries` | List, look up by hash, duplicates |
| POST | `/v1/entries` | New folder, copy |
| GET, PATCH, DELETE | `/v1/entries/{id}` | Read, rename/move/restore, delete |
| GET, PUT | `/v1/entries/{id}/content` | Read, replace bytes |
| GET | `/v1/entries/{id}/preview/{size}.jpg` | One preview |
| PUT | `/v1/entries/{folder}/children/{name}` | New file |
| POST | `/v1/previews` | Many previews, streamed |
| POST | `/v1/batch` | Many requests in one |
| GET | `/v1/changes` | Changes since, or live |
