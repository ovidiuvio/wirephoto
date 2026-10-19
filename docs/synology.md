# wirephoto on a Synology NAS

A step-by-step setup with Container Manager (DSM 7.2 and later), including decoding on the NAS's GPU (Intel
Quick Sync) and the settings that keep an idle NAS quiet.

## 1. What your model can do

wirephoto runs on any Synology model that has Container Manager. What differs is the CPU:

| CPU in the NAS | Image | GPU decoding |
|---|---|---|
| Intel Celeron (J4025, J4125, J3455, ...): DS220+, DS224+, DS420+, DS423+, DS720+, DS920+, DS1520+, DS718+, DS918+, ... | `linux/amd64` | Yes, Intel Quick Sync, if DSM has its GPU driver (step 6) |
| AMD Ryzen (R1600, V1500B, V1780B): DS723+, DS923+, DS1522+, DS1621+, ... | `linux/amd64` | No (no GPU in these CPUs) |
| ARM (Realtek RTD1296, RTD1619B, ...): DS220j, DS223, DS224j, ... | `linux/arm64` | No |

Synology's 2025 models (DS225+, DS425+) are reported to ship without the Intel GPU driver; step 6 shows how to
check yours. Without a usable GPU everything still works, on the CPU.

To see your CPU: **Control Panel → Info Center → General**, or over SSH `cat /proc/cpuinfo | grep "model name" | head -1`.

## 2. Shares and folders

1. **Your photos.** An existing shared folder, e.g. `/volume1/photo`. wirephoto never overwrites or permanently
   deletes files; deletes go to a `.wirephoto-trash` folder inside the library.
2. **wirephoto's own folder**, e.g. `/volume1/docker/wirephoto`, holding the config, the index, the preview cache
   (about 35 GB per million photos) and the logs. Put it on an SSD volume if you have one: browsing reads the
   preview cache all the time.

Create the folder in **File Station** (`docker/wirephoto`).

## 3. The user the container runs as

The container runs as a DSM user, so it reads and writes your photos with that user's permissions. Use the
account that owns the photo share, or create one (e.g. `wirephoto`) and give it read/write on the photo share and
on `docker/wirephoto` (**Control Panel → Shared Folder → Edit → Permissions**).

Its numbers, over SSH (enable it in **Control Panel → Terminal & SNMP**):

```bash
id wirephoto
```

For example `uid=1026(wirephoto) gid=100(users) groups=100(users),101(administrators)`: you need the uid (`1026`),
the gid (`100`), and any extra group that grants access to the share.

## 4. The configuration

Save this as `/volume1/docker/wirephoto/wirephoto.toml` (File Station → upload, or an editor over SSH):

```toml
listen = "0.0.0.0:8443"
data = "/data"

[[library]]
id = "photos"
name = "Photos"
path = "/photos"
read_only = false        # true: browse only
trash_days = 30          # deletes stay in .wirephoto-trash this long; 0 = keep forever
watch = "auto"           # see step 7

[cache]
max_size = "100GB"       # ~35 GB per million photos for the 640 px previews, plus room for larger ones
memory = "1024MB"
pregenerate = true

[decode]
workers = 4              # previews made at once: the number of CPU cores is a good start
# gpu = "on"             # decode on the NAS's GPU (Intel Quick Sync): off unless set; see step 6
```

More than one library: repeat the `[[library]]` block with another `id` and `path` (and another volume line
below).

## 5. The project in Container Manager

**Container Manager → Project → Create**: name `wirephoto`, path `/volume1/docker/wirephoto`, source **Create
docker-compose.yml**, and paste:

```yaml
services:
  wirephoto:
    image: ghcr.io/ovidiuvio/wirephoto:main
    container_name: wirephoto
    user: "1026:100"                 # the uid:gid from step 3
    group_add:
      - "101"                        # extra groups from step 3, if the share needs them
    read_only: true
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    ports:
      - "8443:8443"
    volumes:
      - /volume1/docker/wirephoto/wirephoto.toml:/etc/wirephoto.toml:ro
      - /volume1/docker/wirephoto:/data
      - /volume1/photo:/photos       # your photo share
    environment:
      WIREPHOTO_LOG: "wirephoto=info"
    restart: unless-stopped
```

Folder names with spaces are fine in the volume line when it is quoted:
`- "/volume1/photo-archive/Archive (Glacier):/photos"`.

Click **Next → Done**: Container Manager pulls the image and starts it. The first start lists every folder,
then reads each photo's facts, makes its 640 px preview and hashes it, in the background; follow it with
`wirephoto status` (step 9).

## 6. Decoding on the GPU (Intel Quick Sync)

HEIC photos are the slow ones to preview: a 40 MP HEIC takes seconds on a Celeron. The Celeron's GPU decodes
HEVC (what HEIC is made of) and JPEG in hardware. wirephoto uses it for HEIC, large JPEGs (8 MP and up, also
the ones inside RAW files) and video posters, and keeps everything else on the CPU. It checks the GPU once at
start; anything the GPU turns out not to decode is decoded on the CPU, and a GPU that fails five times in a row
is left alone until the next start. Previews look the same either way (only the decoding moves).

**a. Does DSM have the GPU driver?** Over SSH:

```bash
ls -ln /dev/dri
```

You want `renderD128`, with its group number, for example:

```
crw-rw---- 1 0 937 226, 128 Oct 19 09:00 renderD128
```

Here the group is `937` (DSM's `videodriver` group; yours may differ). No `/dev/dri` at all means no GPU
decoding on this NAS; skip the rest of this step.

**b. Check that a container can use it** (a throwaway container from the wirephoto image, which has the VA-API
tools; `937` is the group from step a):

```bash
sudo docker run --rm --device /dev/dri:/dev/dri --group-add 937 --entrypoint vainfo \
  ghcr.io/ovidiuvio/wirephoto:main --display drm --device /dev/dri/renderD128
```

The list should include `VAProfileHEVCMain` and `VAProfileJPEGBaseline` with `VAEntrypointVLD` (VLD means
decoding).

**c. Give it to wirephoto**: add to the service in the project's `docker-compose.yml` (Container Manager →
Project → wirephoto → Stop, then edit):

```yaml
    devices:
      - /dev/dri:/dev/dri
    group_add:
      - "101"                        # what you had
      - "937"                        # the group of renderD128 from step a
```

Then **Build** (or Start) the project again.

**d. Turn it on** in `wirephoto.toml` (it is off unless set there), then restart the project:

```toml
[decode]
gpu = "on"
```

**e. Check that it is in use.** The log at start says

```
INFO wirephoto: decoding on the GPU: VA-API /dev/dri/renderD128: HEVC, HEVC 10-bit, JPEG
```

and `wirephoto status` has a `GPU:` line saying the same, or why the GPU is not used. Its timing table shows the
GPU's renderings separately (`heic (GPU)`, `jpeg (GPU)`); setting `gpu = "off"` for a while shows the CPU's
(`heic (ffmpeg)`) to compare.

What the `GPU:` line can say:

| `GPU:` | Meaning |
|---|---|
| `VA-API /dev/dri/renderD128: HEVC, ...` | In use, for what it lists |
| `none: no /dev/dri/renderD128 (map /dev/dri into the container)` | The `devices:` lines are missing, or DSM has no GPU driver |
| `not used: ...` | The test pictures did not decode on the GPU; the message says why. Most often the container user is not in the device's group (check `group_add` against `ls -ln /dev/dri`); otherwise compare with step b. Everything runs on the CPU |
| `turned off after 5 failures in a row` | The GPU worked at start and then kept failing; restart the container to try it again |
| `off (turn it on with [decode] gpu = "on")` | Off: the default, or `gpu = "off"` |

A note on the sandbox: wirephoto decodes photos in worker processes that may not create or change files. Using
the GPU needs its device opened read-write, so with a GPU those workers may open existing files read-write
(still not create, truncate or delete them). Where the kernel has no seccomp filters (DSM's often has not; the
log says so at start) this filter is not active anyway; with the GPU off (the default) the sandbox stays strict.

How it decodes a HEIC on the GPU: a phone or camera HEIC is a grid of small tiles (48 for a 12 MP iPhone photo).
wirephoto reads the tiles from the file and sends them to the GPU as one stream, so the whole photo is one
decoding session, and ffmpeg lays the tiles out again. A HEIC the GPU does not take (4:2:2, transparency) is
decoded on the CPU.

## 7. Watching for changes (inotify)

wirephoto notices files added, changed or removed outside it (Synology Drive, SMB, Photos, rsync) by watching each
folder of the library with inotify. DSM allows few watches per user (often 8192); a library with more folders than
that falls back to checking folder times every minute (`poll_seconds`), and the log says so:

```
watch limit reached, polling instead ... (raise fs.inotify.max_user_watches to watch)
```

Synology's own `@eaDir` thumbnail folders are skipped, so they do not count. To raise the limit, add a boot task:
**Control Panel → Task Scheduler → Create → Triggered Task → User-defined script**, user `root`, event
**Boot-up**, script:

```bash
sysctl -w fs.inotify.max_user_watches=524288
```

Run it once by hand (select it → **Run**), then restart the wirephoto project. For a library on an NFS or SMB
mount, set `watch = "poll"`: inotify does not see changes made by other machines.

## 8. HTTPS for browser apps

Browser apps served over `https://` (lith included) can only call an `https://` server. The simplest way on
Synology is DSM's reverse proxy, which also gives HTTP/2 (many previews over one connection):

1. **Control Panel → Login Portal → Advanced → Reverse Proxy → Create**:
   - Source: protocol `HTTPS`, hostname e.g. `photos.yourname.synology.me`, port `443`
   - Destination: protocol `HTTP`, hostname `localhost`, port `8443`
2. **Control Panel → Security → Certificate**: a certificate for that hostname (Let's Encrypt via **Add**), set
   for the reverse proxy entry under **Settings**.

Apps then use `https://photos.yourname.synology.me`. Alternatively give wirephoto a certificate itself (`[tls]`
in the config).

## 9. Keys, and keeping an eye on it

Create a key for each app (it is printed once):

```bash
sudo docker exec wirephoto wirephoto keys create --name lith --scope write --origin https://lith.example.com
```

`--origin` is the address of the web app that will use the key (lith's, not wirephoto's); a key without one is for
scripts. `wirephoto keys list` and `wirephoto keys revoke <name>` manage them.

```bash
sudo docker exec wirephoto wirephoto status
```

shows each library's progress and time left, how long previews take by file type (and decoder), the cache, the
workers and the GPU. The log is in Container Manager (**Container → wirephoto → Log**) and one file a day in
`/volume1/docker/wirephoto/logs` (`wirephoto.log.<date>`), with every request in `access.log.<date>` beside it. `wirephoto pause` and `wirephoto resume` hold and continue the background work,
also across restarts.

## 10. Updating

**Container Manager → Project → wirephoto → Action → Build** pulls the newest `:main` image and recreates the
container; the index and cache stay in `/volume1/docker/wirephoto`. Over SSH:
`cd /volume1/docker/wirephoto && sudo docker compose pull && sudo docker compose up -d` (`docker-compose` on older
DSM). The start line in the
log and `wirephoto status` show the commit the running image was built from.

## 11. When something is off

| Symptom | What to do |
|---|---|
| `permission denied` on the library in the log | The `user`/`group_add` of the container cannot read the share: step 3 |
| Disks never sleep | An idle wirephoto writes nothing; check what else runs (`wirephoto status` should say "up to date") and that the data folder is not on a volume something else keeps busy |
| High CPU while nothing happens | `wirephoto status`: if it is still reading, making previews or hashing, it is working through the library at low priority; if it says "up to date", update the image (older ones spun at idle) |
| Previews of HEIC photos slow | Step 6; `wirephoto status` shows per-type times and whether the GPU is used |
| New files show up only after a minute | The watch limit was reached and it polls: step 7 |
| A browser app cannot connect | It needs `https://` (step 8) and a key made with its `--origin` (step 9) |
