# syntax=docker/dockerfile:1.7
#
# compara in a container: one binary on a distroless base, running as a
# non-root user, with nothing else in the image to patch.
#
#   docker run --rm -v "$PWD:/data" ghcr.io/andrey-usa/compara \
#     compare a.csv b.csv -k id --json -
#
#   docker run --rm -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_REGION \
#     ghcr.io/andrey-usa/compara \
#     compare s3://bucket/july.parquet s3://bucket/august.parquet -k id
#
# The base is gcr.io/distroless/cc: the C library and certificate store the
# binary needs (the same glibc build as the release tarballs) and no shell, no
# package manager. It runs as `nonroot` (65532), works on a read-only root
# filesystem, and writes temporary files -- standard input, decompressed and
# downloaded inputs, the spilling engine's runs -- to /tmp, which under a
# read-only root wants a writable volume (an emptyDir) or TMPDIR pointed at one.
#
# Two ways to fill it:
#   docker build .                         compiles here (the default target)
#   docker build --target release \
#     --build-context dist=<dir> .         takes prebuilt binaries from
#                                          <dir>/<arch>/compara, as the release
#                                          workflow does, so the image holds the
#                                          binary the tarballs hold.

ARG RUST_VERSION=1.90

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS build
ARG TARGETARCH
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Cross-compiling for the other architecture needs its linker; a native build
# needs nothing.
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported architecture $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$(uname -m)" != "$(echo $target | cut -d- -f1)" ]; then \
      apt-get update && apt-get install -y --no-install-recommends \
        "gcc-$(echo $target | cut -d- -f1 | sed 's/_/-/')-linux-gnu" "libc6-dev-${TARGETARCH}-cross"; \
      rustup target add "$target"; \
      export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
             CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
             CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
             CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc; \
    fi; \
    cargo build --release --locked --bin compara --target "$target"; \
    cp "target/$target/release/compara" /compara

FROM gcr.io/distroless/cc-debian12:nonroot AS base
LABEL org.opencontainers.image.title="compara" \
      org.opencontainers.image.description="Compare two tables (CSV, NDJSON, Parquet; local, s3://, gs://, az://, https://) on a composite key" \
      org.opencontainers.image.source="https://github.com/andrey-usa/compara" \
      org.opencontainers.image.licenses="MIT"
WORKDIR /data
ENTRYPOINT ["/usr/local/bin/compara"]
CMD ["--help"]

# Prebuilt: the release workflow's binaries, one per architecture.
FROM base AS release
ARG TARGETARCH
COPY --from=dist --chmod=0755 ${TARGETARCH}/compara /usr/local/bin/compara

# The default: compiled above.
FROM base AS source
COPY --from=build --chmod=0755 /compara /usr/local/bin/compara
