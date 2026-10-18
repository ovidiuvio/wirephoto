#!/bin/sh
# Refreshes crates/jpegli-sys/vendor from github.com/google/jpegli at the commit below: the encoder sources, its
# libjpeg headers, and Highway (SIMD dispatch), pruned to what the static library needs.
#
#   scripts/vendor-jpegli.sh [COMMIT]
set -eu
COMMIT=${1:-031a0077f5799a6041004267fc12b956c1f52a20}
DEST=$(cd "$(dirname "$0")/.." && pwd)/crates/jpegli-sys/vendor
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

git -C "$TMP" init -q jpegli
git -C "$TMP/jpegli" fetch -q --depth 1 https://github.com/google/jpegli.git "$COMMIT"
git -C "$TMP/jpegli" checkout -q FETCH_HEAD
git -C "$TMP/jpegli" submodule update -q --init --depth 1 third_party/highway third_party/libjpeg-turbo
SRC=$TMP/jpegli
HWY=$SRC/third_party/highway

rm -rf "$DEST"
mkdir -p "$DEST/jpegli/lib" "$DEST/jpegli/third_party/libjpeg-turbo" "$DEST/highway"
cp "$SRC/LICENSE" "$SRC/PATENTS" "$DEST/jpegli/"
cp -R "$SRC/lib/base" "$SRC/lib/jpegli" "$SRC/lib/jpegli_lists.cmake" "$DEST/jpegli/lib/"
rm -f "$DEST"/jpegli/lib/jpegli/*test* "$DEST"/jpegli/lib/jpegli/libjpeg_wrapper.cc
for f in jconfig.h.in jpeglib.h jmorecfg.h LICENSE.md README.ijg; do
  cp "$SRC/third_party/libjpeg-turbo/$f" "$DEST/jpegli/third_party/libjpeg-turbo/"
done
cp -R "$HWY/CMakeLists.txt" "$HWY/LICENSE" "$HWY/cmake" "$HWY/hwy" "$HWY"/*.pc.in "$DEST/highway/"
rm -rf "$DEST/highway/hwy/contrib"
find "$DEST/highway/hwy" -name '*_test.cc' -delete
{
  echo "jpegli  $COMMIT"
  echo "highway $(git -C "$HWY" rev-parse HEAD)"
  echo "libjpeg-turbo headers $(git -C "$SRC/third_party/libjpeg-turbo" rev-parse HEAD)"
} > "$DEST/VERSIONS"
du -sh "$DEST"
