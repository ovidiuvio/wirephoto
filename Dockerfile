# syntax=docker/dockerfile:1
#
# wirephoto: one image for linux/amd64 and linux/arm64 (CI builds each natively and merges them).

FROM rust:1-trixie AS build
# libjpeg-turbo is built from source (Debian ships 2.x; the TurboJPEG 3 API is needed), with SIMD (nasm on
# x86_64; NEON needs nothing extra on arm64). jpegli and Highway are built from crates/jpegli-sys/vendor (C++,
# cmake; SIMD targets picked at runtime). libheif comes from Debian.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake g++ nasm pkg-config libheif-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ENV TURBOJPEG_SOURCE=vendor \
    TURBOJPEG_STATIC=1 \
    CARGO_TARGET_DIR=/src/target
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p wirephoto \
 && cp /src/target/release/wirephoto /usr/local/bin/wirephoto

FROM debian:trixie-slim
# HEIF/HEIC (libde265) and AVIF (dav1d) decoding; ffmpeg and ffprobe for video posters and durations (the server
# runs without them, minus those); tini reaps the decode workers and forwards signals.
RUN apt-get update \
 && apt-get install -y --no-install-recommends libheif1 libheif-plugin-libde265 libheif-plugin-dav1d ffmpeg tini ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /data && chown 1000:1000 /data
COPY --from=build /usr/local/bin/wirephoto /usr/local/bin/wirephoto
COPY wirephoto.example.toml /etc/wirephoto.toml
ENV WIREPHOTO_CONFIG=/etc/wirephoto.toml
USER 1000:1000
VOLUME /data
EXPOSE 8443
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["wirephoto", "health"]
ENTRYPOINT ["tini", "--", "wirephoto"]
CMD ["serve"]
