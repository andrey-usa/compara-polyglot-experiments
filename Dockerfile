# primavera with a headless Chromium, for CI and Kubernetes/OpenShift Jobs.
# Build from the directory that holds both checkouts (primavera links
# navigera from ../navigera):
#
#   docker build -f primavera/Dockerfile -t primavera .
#   docker run --rm -v "$PWD/tests:/work" primavera run . --base-url http://host:3000
#
# amd64 and arm64 (Debian's chromium-headless-shell; Chrome for Testing has
# no Linux arm64 build). Runs as an unprivileged user (uid 10001, any uid
# works: OpenShift's random ones too) without Chrome's sandbox: in a
# container without user namespaces Chrome can't start one, and the
# container is the boundary.

FROM rust:1-trixie AS build
WORKDIR /src
COPY navigera/Cargo.toml navigera/Cargo.lock navigera/
COPY navigera/src navigera/src
COPY navigera/skills navigera/skills
COPY primavera/Cargo.toml primavera/Cargo.lock primavera/
COPY primavera/src primavera/src
COPY primavera/skills primavera/skills
COPY primavera/vendor primavera/vendor
RUN cd primavera && cargo build --release --locked && strip target/release/primavera

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends chromium-headless-shell fonts-liberation fonts-noto-color-emoji ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 10001 --create-home --home-dir /home/pv pv \
 && mkdir /work && chmod 1777 /work
COPY --from=build /src/primavera/target/release/primavera /usr/local/bin/primavera
ENV NAVIGERA_CHROMIUM=/usr/bin/chromium-headless-shell \
    NAVIGERA_NO_SANDBOX=1 \
    HOME=/home/pv
USER 10001
WORKDIR /work
ENTRYPOINT ["primavera"]
CMD ["run", "."]
