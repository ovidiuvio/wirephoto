#!/usr/bin/env python3
"""Compare libjpeg-turbo, mozjpeg and jpegli for 640 px previews: size at equal SSIMULACRA2 quality, and speed.

    python3 scripts/bench-jpeg.py CORPUS [OUT]

CORPUS is a folder of photos (RAWs, JPEGs, PNGs; searched recursively): RAWs contribute their embedded preview,
images are used as they are. OUT (default ./bench-out) receives the references and encodes; delete it to start
over. Needs Pillow, exiftool, and the encoders, located by environment variable or on PATH:

    TURBO_CJPEG   libjpeg-turbo's cjpeg    (Homebrew: /opt/homebrew/opt/jpeg-turbo/bin/cjpeg)
    MOZ_CJPEG     mozjpeg's cjpeg          (Homebrew: /opt/homebrew/opt/mozjpeg/bin/cjpeg)
    CJPEGLI       jpegli's cjpegli         (build github.com/google/jpegli; not in Homebrew's jpeg-xl)
    SSIMULACRA2   libjxl's ssimulacra2     (Homebrew jpeg-xl)
"""
import glob, os, shutil, statistics, subprocess, sys, time
from PIL import Image


CORPUS = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else sys.exit(__doc__)
B = os.path.abspath(sys.argv[2] if len(sys.argv) > 2 else 'bench-out')
TURBO = os.environ.get('TURBO_CJPEG', '/opt/homebrew/opt/jpeg-turbo/bin/cjpeg')
MOZ = os.environ.get('MOZ_CJPEG', '/opt/homebrew/opt/mozjpeg/bin/cjpeg')
JPEGLI = os.environ.get('CJPEGLI') or shutil.which('cjpegli') or 'cjpegli'
SSIM2 = os.environ.get('SSIMULACRA2') or shutil.which('ssimulacra2') or '/opt/homebrew/opt/jpeg-xl/bin/ssimulacra2'
RAW = ('.arw', '.cr2', '.cr3', '.dng', '.nef', '.orf', '.raf', '.rw2', '.pef', '.srw')
IMAGE = ('.jpg', '.jpeg', '.png', '.tif', '.tiff')
EDGE = 640
QUALITIES = [50, 60, 65, 70, 75, 80, 85, 90, 95]
TARGETS = [70, 80, 85]

ENCODERS = {
    'turbo (baseline, optimized)': lambda q, ppm, png, out: [TURBO, '-quality', str(q), '-optimize', '-outfile', out, ppm],
    'mozjpeg (progressive)':       lambda q, ppm, png, out: [MOZ, '-quality', str(q), '-outfile', out, ppm],
    'mozjpeg (baseline)':          lambda q, ppm, png, out: [MOZ, '-quality', str(q), '-baseline', '-outfile', out, ppm],
    'jpegli 4:2:0 (progressive)':  lambda q, ppm, png, out: [JPEGLI, png, out, '-q', str(q), '--chroma_subsampling=420'],
    'jpegli 4:2:0 (baseline)':     lambda q, ppm, png, out: [JPEGLI, png, out, '-q', str(q), '--chroma_subsampling=420', '-p', '0'],
    'jpegli 4:4:4 (progressive)':  lambda q, ppm, png, out: [JPEGLI, png, out, '-q', str(q)],
}


def run(cmd):
    subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def timed(cmd, reps=3):
    best = 1e9
    for _ in range(reps):
        t = time.perf_counter(); run(cmd); best = min(best, time.perf_counter() - t)
    return best


def extract_sources(src):
    os.makedirs(src, exist_ok=True)
    for f in sorted(glob.glob(CORPUS + '/**/*', recursive=True)):
        name, ext = os.path.splitext(os.path.basename(f))
        ext = ext.lower()
        if ext in RAW:
            best = None
            for tag in ('JpgFromRaw', 'PreviewImage', 'OtherImage'):
                data = subprocess.run(['exiftool', '-b', '-' + tag, f], capture_output=True).stdout
                if data[:2] == b'\xff\xd8' and (best is None or len(data) > len(best)):
                    best = data
            if best:
                open(f'{src}/{name}.jpg', 'wb').write(best)
        elif ext in IMAGE:
            shutil.copy(f, f'{src}/{name}{ext}')


def make_refs(src, ref):
    os.makedirs(ref, exist_ok=True)
    names = []
    for f in sorted(glob.glob(src + '/*')):
        name = os.path.splitext(os.path.basename(f))[0]
        im = Image.open(f).convert('RGB')
        if max(im.size) < EDGE * 1.5:
            continue  # a preview too small to make a fair 640 px reference from
        im.thumbnail((EDGE, EDGE), Image.LANCZOS)
        im.save(f'{ref}/{name}.png'); im.save(f'{ref}/{name}.ppm')
        names.append(name)
    return names


def score(ref_png, jpg):
    out = subprocess.run([SSIM2, ref_png, jpg], capture_output=True, text=True).stdout.strip().split()
    return float(out[-1])


def size_at(curve, target):
    """Bytes at a target score, interpolated on the (score, bytes) curve; None if out of range."""
    pts = sorted(curve)
    for (s0, b0), (s1, b1) in zip(pts, pts[1:]):
        if s0 <= target <= s1 and s1 > s0:
            return b0 + (b1 - b0) * (target - s0) / (s1 - s0)
    return None


def main():
    src, ref, out = B + '/src', B + '/ref', B + '/out'
    if not os.path.isdir(src):
        extract_sources(src)
    names = make_refs(src, ref)
    os.makedirs(out, exist_ok=True)
    print(f'{len(names)} images: {", ".join(names)}\n', flush=True)

    curves = {e: {n: [] for n in names} for e in ENCODERS}
    times = {e: [] for e in ENCODERS}
    for e, mk in ENCODERS.items():
        for n in names:
            for q in QUALITIES:
                o = f'{out}/{n}.{abs(hash(e))}.{q}.jpg'
                cmd = mk(q, f'{ref}/{n}.ppm', f'{ref}/{n}.png', o)
                t = timed(cmd, reps=3 if q == 80 else 1)
                if q == 80:
                    times[e].append(t)
                curves[e][n].append((score(f'{ref}/{n}.png', o), os.path.getsize(o)))
        print(f'  encoded: {e}', flush=True)

    # process start-up cost, to subtract from the per-thumbnail times
    tiny_ppm, tiny_png = out + '/tiny.ppm', out + '/tiny.png'
    Image.new('RGB', (8, 8)).save(tiny_ppm); Image.new('RGB', (8, 8)).save(tiny_png)
    startup = {e: timed(mk(80, tiny_ppm, tiny_png, out + '/tiny.jpg'), reps=5) for e, mk in ENCODERS.items()}

    base = 'turbo (baseline, optimized)'
    print('\nSize at matched quality (SSIMULACRA2), relative to libjpeg-turbo; mean bytes per 640 px thumbnail')
    print(f'{"encoder":30}' + ''.join(f'{"score " + str(t):>22}' for t in TARGETS))
    for e in ENCODERS:
        row = f'{e:30}'
        for t in TARGETS:
            ratios, sizes = [], []
            for n in names:
                a, b = size_at(curves[e][n], t), size_at(curves[base][n], t)
                if a and b:
                    ratios.append(a / b); sizes.append(a)
            if ratios:
                gm = statistics.geometric_mean(ratios)
                row += f'{f"{(gm - 1) * 100:+.1f}%  {statistics.mean(sizes) / 1024:.1f} KB":>22}'
            else:
                row += f'{"n/a":>22}'
        print(row)

    print('\nMean SSIMULACRA2 and size per quality setting (to pick the setting that lands on a target score)')
    print(f'{"encoder":30}' + ''.join(f'{"q" + str(q):>13}' for q in QUALITIES))
    for e in ENCODERS:
        cells = []
        for i, q in enumerate(QUALITIES):
            pts = [curves[e][n][i] for n in names]
            cells.append(f'{statistics.mean(p[0] for p in pts):5.1f} {statistics.mean(p[1] for p in pts) / 1024:5.1f}K')
        print(f'{e:30}' + ''.join(f'{c:>13}' for c in cells))

    print('\nEncode time per 640 px thumbnail at q80 (median over images, process start-up subtracted)')
    for e in ENCODERS:
        print(f'{e:30} {max(0, statistics.median(times[e]) - startup[e]) * 1000:6.2f} ms')

    # throughput on a full-size image, where start-up is negligible
    big = max(glob.glob(src + '/*'), key=lambda f: Image.open(f).size[0] * Image.open(f).size[1])
    im = Image.open(big).convert('RGB'); mp = im.size[0] * im.size[1] / 1e6
    im.save(out + '/big.ppm'); im.save(out + '/big.png')
    print(f'\nFull-size encode, {os.path.basename(big)} {im.size[0]}x{im.size[1]} ({mp:.1f} MP), q80')
    for e, mk in ENCODERS.items():
        t = timed(mk(80, out + '/big.ppm', out + '/big.png', out + '/big.jpg'), reps=3) - startup[e]
        print(f'{e:30} {t * 1000:7.0f} ms  {mp / t:6.1f} MP/s')


if __name__ == '__main__':
    sys.exit(main())
