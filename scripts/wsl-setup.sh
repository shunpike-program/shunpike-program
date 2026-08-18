#!/usr/bin/env bash
# One-shot toolchain setup for the Ubuntu WSL2 distro (run INSIDE Ubuntu).
# Idempotent: safe to re-run if a step fails partway.
#
# Installs: build deps, rustup (stable), Solana CLI (Agave), Anchor via avm.
# Docker/solana-verify are NOT installed here — verified builds only matter at
# the mainnet deploy (P6) and use Docker Desktop on the Windows side.
set -euo pipefail

echo "==> apt build dependencies"
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev libudev-dev curl git

if ! command -v cargo >/dev/null 2>&1; then
  echo "==> rustup (stable)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"

if ! command -v solana >/dev/null 2>&1; then
  echo "==> Solana CLI (Agave stable)"
  sh -c "$(curl -sSfL https://release.anza.xyz/stable/install)"
  # The installer appends PATH to the shell profile; make it live for this run.
  export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"
  if ! grep -q 'solana/install/active_release' "$HOME/.bashrc"; then
    echo 'export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"' >> "$HOME/.bashrc"
  fi
fi

if ! command -v avm >/dev/null 2>&1; then
  echo "==> avm (Anchor version manager)"
  cargo install --git https://github.com/coral-xyz/anchor avm --force
fi

# Pinned to match Anchor.toml's [toolchain] anchor_version.
echo "==> anchor 0.31.1"
avm install 0.31.1
avm use 0.31.1

echo "==> versions"
rustc --version
solana --version
anchor --version

echo "==> done. Next: cd to the repo (e.g. /mnt/c/Users/dkbut/source/repos/shunpike-program) && anchor build && cargo test"
