#!/bin/sh
# The object-storage stack the cloud inputs are built on.
set -eu
cargo add object_store --no-default-features --features aws,gcp,azure,http
cargo add tokio --no-default-features --features rt-multi-thread
cargo add futures --no-default-features --features std
cargo add bytes url
