# syntax=docker/dockerfile:1

FROM rust:1-slim-trixie AS build
ARG TARGETARCH
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Cache the crate registry and build directory between builds: a rebuild
# after a source change then only recompiles Inkult itself, which matters
# on small homelab hardware. The binary is copied out
# because cache mounts are not part of the image.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=inkult-target-${TARGETARCH},target=/src/target,sharing=locked \
    cargo build --release --locked \
 && cp target/release/inkult /usr/local/bin/inkult

FROM debian:trixie-slim

# ffmpeg: remuxing extracted audio and converting/cropping artwork.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl ffmpeg \
 && rm -rf /var/lib/apt/lists/*

# yt-dlp standalone build, verified against checksums pinned here so a
# tampered or truncated download fails the build. YouTube changes often:
# when downloads start failing, bump the version and both checksums (from
# the release's SHA2-256SUMS file).
ARG YTDLP_VERSION=2026.08.19
ARG YTDLP_SHA256_AMD64=58162f9bfdc27458ea47bfcb311cf47028f17d8154a8bf7d689861d46399230a
ARG YTDLP_SHA256_ARM64=b16e4dab368a816cd05d477d698a605a6ae87ccee1c8ffd38fa21d7254141fcc
ARG TARGETARCH
RUN case "${TARGETARCH:-amd64}" in \
      amd64) asset=yt-dlp_linux; sum="${YTDLP_SHA256_AMD64}" ;; \
      arm64) asset=yt-dlp_linux_aarch64; sum="${YTDLP_SHA256_ARM64}" ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && curl -fsSL -o /usr/local/bin/yt-dlp \
      "https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/${asset}" \
 && echo "${sum}  /usr/local/bin/yt-dlp" | sha256sum -c - \
 && chmod 0755 /usr/local/bin/yt-dlp

# yt-dlp needs a JavaScript runtime to solve YouTube's player challenges.
# Pinned rather than a moving tag, so a rebuild cannot silently pull a
# different binary.
COPY --from=denoland/deno:bin-2.9.7 /deno /usr/local/bin/deno

COPY --from=build /usr/local/bin/inkult /usr/local/bin/inkult

RUN useradd --create-home --uid 1000 inkult \
 && mkdir -p /music /data \
 && chown inkult:inkult /music /data
USER inkult

# yt-dlp and deno keep caches under $HOME by default. The container usually
# runs as the host user's UID (PUID), which has no home directory here, so
# point them at the data volume, which that user can always write.
ENV INKULT_BIND=0.0.0.0:9876 \
    INKULT_LIBRARY_DIR=/music \
    INKULT_DATA_DIR=/data \
    XDG_CACHE_HOME=/data/cache \
    DENO_DIR=/data/cache/deno
VOLUME ["/music", "/data"]
# 9876: web UI. 2234-2238: Soulseek peer connections (INKULT_SOULSEEK_PORTS).
EXPOSE 9876 2234-2238
HEALTHCHECK --interval=60s --timeout=5s --start-period=30s CMD curl -fsS http://127.0.0.1:9876/api/auth/state >/dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/inkult"]
