#!/usr/bin/env python3
"""Benchmark the wirephoto API on large photos: indexing, listings, previews (cold and cached, single, streamed,
concurrent), originals and uploads. Starts the server itself on a fresh index each run.

    python3 scripts/bench-api.py --bin <wirephoto binary> --png A.png --png B.png --heic A.heic --raw X.nef [--out DIR]

PNGs are turned into quality-92 JPEGs (camera-like originals). Each source is copied several times so every
cold measurement renders an entry nobody rendered before. Needs Pillow. Talks plain HTTP on loopback, so it
measures the server, not a network.
"""
import argparse, http.client, json, os, shutil, statistics, subprocess, sys, threading, time
from concurrent.futures import ThreadPoolExecutor
from PIL import Image

PORT = 8790
SIZES = [160, 640, 1280, 2560]


def ms(x):
    return f"{x * 1000:.1f} ms"


def pct(values, p):
    v = sorted(values)
    return v[min(len(v) - 1, int(round(p / 100 * (len(v) - 1))))]


class Client:
    def __init__(self, key):
        self.key = key
        self.local = threading.local()

    def conn(self):
        c = getattr(self.local, "c", None)
        if c is None:
            c = self.local.c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=300)
        return c

    def req(self, method, path, body=None, headers=None):
        h = {"Authorization": f"Bearer {self.key}"}
        h.update(headers or {})
        c = self.conn()
        t = time.perf_counter()
        c.request(method, path, body=body, headers=h)
        r = c.getresponse()
        data = r.read()
        return r.status, dict(r.getheaders()), data, time.perf_counter() - t

    def json(self, path):
        s, _, d, _ = self.req("GET", path)
        assert s == 200, (s, d[:200])
        return json.loads(d)


def prepare(args, lib):
    shutil.rmtree(lib, ignore_errors=True)
    for d in ("jpeg", "heic", "raw", "upload-src"):
        os.makedirs(os.path.join(lib, d) if d != "upload-src" else os.path.join(args.out, d), exist_ok=True)
    jpegs = []
    for i, png in enumerate(args.png):
        dst = os.path.join(args.out, f"src-{i}.jpg")
        if not os.path.exists(dst):
            Image.open(png).convert("RGB").save(dst, quality=92)
        jpegs.append(dst)
    for kind, files, copies in (("jpeg", jpegs, args.copies), ("heic", args.heic, args.copies // 2), ("raw", args.raw, args.copies // 2)):
        for i, src in enumerate(files):
            ext = os.path.splitext(src)[1]
            for c in range(copies):
                shutil.copyfile(src, os.path.join(lib, kind, f"{kind}-{i}-{c:03d}{ext}"))
    return jpegs


def start(args, data, pregenerate):
    cfg = os.path.join(args.out, "bench.toml")
    with open(cfg, "w") as f:
        f.write(f'listen = "127.0.0.1:{PORT}"\ndata = "{data}"\n\n[[library]]\nid = "bench"\nname = "Bench"\n'
                f'path = "{os.path.join(args.out, "lib")}"\nwatch = "poll"\npoll_seconds = 3600\n\n'
                f'[cache]\nmax_size = "20GB"\npregenerate = {"true" if pregenerate else "false"}\n')
    shutil.rmtree(data, ignore_errors=True)
    key = subprocess.run([args.bin, "-c", cfg, "keys", "create", "--name", "bench", "--scope", "write"],
                         capture_output=True, text=True, check=True).stdout.strip()
    env = dict(os.environ, WIREPHOTO_LOG="warn")
    proc = subprocess.Popen([args.bin, "-c", cfg, "serve"], env=env, stdout=subprocess.DEVNULL, stderr=open(os.path.join(args.out, "server.log"), "w"))
    t0 = time.perf_counter()
    for _ in range(200):
        try:
            http.client.HTTPConnection("127.0.0.1", PORT, timeout=1).request("GET", "/healthz")
            break
        except OSError:
            time.sleep(0.05)
    return proc, Client(key), t0


def files_in(c, lib_id):
    out, cursor = [], None
    while True:
        q = f"/v1/entries?library={lib_id}&kind=image,raw&limit=1000" + (f"&cursor={cursor}" if cursor else "")
        page = c.json(q)
        out += page["items"]
        cursor = page.get("next")
        if not cursor:
            return out


def bench_indexing(args, n_files):
    proc, c, t0 = start(args, os.path.join(args.out, "data-index"), pregenerate=True)
    try:
        marks = {}
        while True:
            lib = c.json("/v1/libraries")[0]
            st = (lib.get("indexing") or {}).get("state")
            now = time.perf_counter() - t0
            if st and st not in marks:
                marks[st] = now
            files = files_in(c, "lib_bench") if lib.get("root") else []
            facts = sum(1 for e in files if e.get("media"))
            hashed = sum(1 for e in files if e.get("sha256"))
            if "listed" not in marks and len(files) >= n_files:
                marks["listed"] = now
            if "facts" not in marks and facts >= n_files:
                marks["facts"] = now
            if hashed >= n_files:
                marks["done"] = now
                break
            time.sleep(0.1)
        # Previews are made after facts and before hashes, so "done" includes every 640 px preview.
        return marks
    finally:
        proc.terminate()
        proc.wait()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--png", action="append", default=[])
    ap.add_argument("--heic", action="append", default=[])
    ap.add_argument("--raw", action="append", default=[])
    ap.add_argument("--copies", type=int, default=20)
    ap.add_argument("--out", default="bench-out/api")
    args = ap.parse_args()
    args.out = os.path.abspath(args.out)
    os.makedirs(args.out, exist_ok=True)
    lib = os.path.join(args.out, "lib")
    jpegs = prepare(args, lib)
    n_files = sum(len(fs) for _, _, fs in os.walk(lib))
    sizes = {k: os.path.getsize(os.path.join(lib, k, sorted(os.listdir(os.path.join(lib, k)))[0])) for k in ("jpeg", "heic", "raw")}
    dims = Image.open(jpegs[0]).size
    print(f"# wirephoto API benchmark\n")
    print(f"{n_files} files: 40 MP JPEG {dims[0]}x{dims[1]} (~{sizes['jpeg'] / 1e6:.1f} MB), HEIC (~{sizes['heic'] / 1e6:.1f} MB), "
          f"RAW (~{sizes['raw'] / 1e6:.1f} MB); loopback HTTP/1.1; release build\n")

    # ---- indexing a fresh library ----
    m = bench_indexing(args, n_files)
    print("## Indexing a new library (pre-generation on)\n")
    print(f"| Stage | Done after |\n|---|---|")
    print(f"| Folders and names listed | {m.get('listed', 0):.2f} s |")
    print(f"| Media facts for every file | {m.get('facts', 0):.2f} s |")
    print(f"| 640 px preview + SHA-256 of every file | {m['done']:.2f} s ({n_files / m['done']:.1f} files/s) |\n")

    # ---- on-demand work, pre-generation off ----
    proc, c, _ = start(args, os.path.join(args.out, "data-cold"), pregenerate=False)
    try:
        while True:
            files = files_in(c, "lib_bench")
            if len(files) >= n_files and all(e.get("media") for e in files):
                break
            time.sleep(0.1)
        by_kind = {"jpeg": [], "heic": [], "raw": []}
        for e in files:
            by_kind[e["path"].split("/")[1]].append(e)
        for k in by_kind:
            by_kind[k].sort(key=lambda e: e["name"])
        folder = c.json("/v1/entries?library=lib_bench&path=/jpeg")["items"][0]["id"]

        lat = [c.req("GET", f"/v1/entries?parent={folder}&limit=200")[3] for _ in range(50)]
        print("## Listing\n")
        print(f"| Request | p50 | p95 |\n|---|---|---|")
        print(f"| A folder of {len(by_kind['jpeg'])} entries with facts and signed URLs | {ms(statistics.median(lat))} | {ms(pct(lat, 95))} |\n")

        print("## One preview (sequential requests; sizes below 640 first render the missing 640)\n")
        print("| Source | Size | First request (rendered) | Again (cached) | Bytes |\n|---|---|---|---|---|")
        used = {k: 0 for k in by_kind}
        with_640 = {}
        label = {"jpeg": "40 MP JPEG", "heic": "40 MP HEIC", "raw": "RAW (NEF, full-size embedded JPEG)"}
        for k in ("jpeg", "heic", "raw"):
            for size in SIZES:
                cold, warm, nbytes = [], [], 0
                for e in by_kind[k][used[k]:used[k] + 3]:
                    url = e["urls"]["preview"].replace("{size}", str(size))
                    s, _, d, t = c.req("GET", url)
                    assert s == 200, (s, d[:200])
                    cold.append(t)
                    nbytes = len(d)
                    warm += [c.req("GET", url)[3] for _ in range(5)]
                if size == 640:
                    with_640[k] = by_kind[k][used[k]:used[k] + 3]
                used[k] += 3
                print(f"| {label[k]} | {size} | {ms(statistics.median(cold))} | {ms(statistics.median(warm))} | {nbytes / 1024:.0f} KB |")
        print()

        print("## Smaller sizes once the 640 px preview is cached (made from it, first request)\n")
        print("| Source | 160 | 320 | 480 |\n|---|---|---|---|")
        for k in ("jpeg", "heic", "raw"):
            row = []
            for size in (160, 320, 480):
                times = []
                for e in with_640[k]:
                    s, _, d, t = c.req("GET", e["urls"]["preview"].replace("{size}", str(size)))
                    assert s == 200, (s, d[:200])
                    times.append(t)
                row.append(ms(statistics.median(times)))
            print(f"| {label[k]} | " + " | ".join(row) + " |")
        print()

        print("## Many previews in one request (`POST /v1/previews`, 640 px)\n")
        print("| Request | First frame | All frames | Throughput |\n|---|---|---|---|")
        for k in ("jpeg", "heic"):
            batch = [e["id"] for e in by_kind[k][used[k]:]]
            for state in ("rendered", "cached"):
                body = json.dumps({"ids": batch, "size": 640})
                conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=600)
                t = time.perf_counter()
                conn.request("POST", "/v1/previews", body=body, headers={"Authorization": f"Bearer {c.key}", "Content-Type": "application/json"})
                r = conn.getresponse()
                first, frames = None, 0
                while True:
                    head = r.read(12)
                    if len(head) < 12:
                        break
                    if first is None:
                        first = time.perf_counter() - t
                    r.read(int.from_bytes(head[8:12], "little"))
                    frames += 1
                total = time.perf_counter() - t
                print(f"| {len(batch)} x {label[k]}, {state} | {ms(first)} | {ms(total)} | {frames / total:.0f} previews/s |")
        print()

        warm_urls = [e["urls"]["preview"].replace("{size}", "640") for e in by_kind["jpeg"][used["jpeg"]:]]
        print("## Cached previews under concurrency (640 px, keep-alive connections)\n")
        print("| Clients | Requests/s | p50 | p99 |\n|---|---|---|---|")
        for clients in (1, 8, 32):
            lats, lock = [], threading.Lock()

            def worker(i):
                local = []
                for j in range(200):
                    local.append(c.req("GET", warm_urls[(i + j) % len(warm_urls)])[3])
                with lock:
                    lats.extend(local)

            t = time.perf_counter()
            with ThreadPoolExecutor(clients) as ex:
                list(ex.map(worker, range(clients)))
            total = time.perf_counter() - t
            print(f"| {clients} | {len(lats) / total:.0f} | {ms(statistics.median(lats))} | {ms(pct(lats, 99))} |")
        print()

        print("## Originals\n")
        print("| Request | Time | Throughput |\n|---|---|---|")
        for k in ("jpeg", "heic", "raw"):
            url = by_kind[k][0]["urls"]["content"]
            times = []
            for _ in range(5):
                s, _, d, t = c.req("GET", url)
                times.append(t)
            print(f"| Whole {label[k]} ({len(d) / 1e6:.1f} MB) | {ms(statistics.median(times))} | {len(d) / statistics.median(times) / 1e6:.0f} MB/s |")
        url = by_kind["raw"][0]["urls"]["content"]
        rt = [c.req("GET", url, headers={"Range": f"bytes={i * 65536}-{i * 65536 + 65535}"})[3] for i in range(50)]
        print(f"| 64 KB range of a RAW | {ms(statistics.median(rt))} | |\n")

        print("## Uploads (`PUT .../children/{name}`: stream to disk, SHA-256, facts, fsync, publish)\n")
        root_jpeg = folder
        data = open(jpegs[0], "rb").read()
        times = []
        for i in range(5):
            s, _, d, t = c.req("PUT", f"/v1/entries/{root_jpeg}/children/upload-{i}.jpg", body=data, headers={"If-None-Match": "*"})
            assert s == 201, (s, d[:300])
            times.append(t)
        print("| Upload | Time | Throughput |\n|---|---|---|")
        print(f"| 40 MP JPEG ({len(data) / 1e6:.1f} MB), until 201 | {ms(statistics.median(times))} | {len(data) / statistics.median(times) / 1e6:.0f} MB/s |\n")
    finally:
        proc.terminate()
        proc.wait()


if __name__ == "__main__":
    sys.exit(main())
