#!/usr/bin/env bash
set -euo pipefail

# Compile on the oldest supported server baseline, not the GitHub runner's
# newer glibc. AlmaLinux 9 provides glibc 2.34 and OpenSSL 3.
docker run --rm \
  --volume "${PWD}:/workspace" --workdir /workspace \
  --env TWM_RELEASE_REPO --env BAF_NOTIFY_RELAY_URL \
  --env BAF_NOTIFY_SECRET --env BAF_BACKEND_TOKEN \
  --env "HOST_UID=$(id -u)" --env "HOST_GID=$(id -g)" \
  almalinux:9 bash -euo pipefail -c '
    dnf -y install gcc gcc-c++ make cmake perl pkgconf-pkg-config \
      openssl-devel ca-certificates git
    curl --proto "=https" --tlsv1.2 --fail --silent --show-error \
      https://sh.rustup.rs | sh -s -- -y --profile minimal \
        --default-toolchain nightly-2026-08-19
    . "$HOME/.cargo/env"
    CARGO_TARGET_DIR=/workspace/target/el9 cargo build --locked --release \
      --target x86_64-unknown-linux-gnu
    install -d target/x86_64-unknown-linux-gnu/release
    install -m 755 target/el9/x86_64-unknown-linux-gnu/release/twm \
      target/x86_64-unknown-linux-gnu/release/twm
    install -m 755 target/el9/x86_64-unknown-linux-gnu/release/TWM-loader \
      target/x86_64-unknown-linux-gnu/release/TWM-loader
    chown -R "$HOST_UID:$HOST_GID" target
  '
