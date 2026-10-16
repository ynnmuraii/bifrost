#!/usr/bin/env bash
# gate-linux.sh — the Linux side of the workspace gate. Runs on a Linux host (the laptop or
# the CI runner), never on the PC.
#
# Mirrors .github/workflows/ci.yml: fmt, clippy (-D warnings), a workspace build and the
# workspace tests under Xvfb. The old `nm` symbol check over the agent binary is gone on
# purpose: since b23335a the agent is not a binary of its own (the one bifrost
# executable links Winsock/sockets for the core), so the agent's no-socket rule is proved by
# apps/lws-agent/tests/no_network.rs — part of `cargo test --workspace` below.
#
# Usage:
#   ./gate-linux.sh                 # evidence in target/live-e2e/runs/<timestamp>/
#   ./gate-linux.sh <evidence-dir>
set -euo pipefail
cd "$(dirname "$0")/../../.."

stamp=$(date +%Y%m%d-%H%M%S)
evidence=${1:-target/live-e2e/runs/$stamp}
mkdir -p "$evidence"
exec > >(tee "$evidence/gate-linux.txt") 2>&1

apt-get update -qq
apt-get install -y -qq --no-install-recommends build-essential binutils xvfb xauth libgl1 python3
rustup component add rustfmt clippy
cargo fmt --all --check
cargo clippy --workspace --all-targets --target-dir target/live-e2e/linux-target -- -D warnings
cargo build --workspace --target-dir target/live-e2e/linux-target
xvfb-run -a cargo test --workspace --target-dir target/live-e2e/linux-target
printf 'PASS Linux full gate\n'
