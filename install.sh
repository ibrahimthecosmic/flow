#!/bin/sh
# Copyright 2026 the Flow authors. MIT license.
# Install the flow binary from GitHub release assets.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/ibrahimthecosmic/flow/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/ibrahimthecosmic/flow/main/install.sh | sh -s v2.9.0
#
# Environment variables:
#   FLOW_INSTALL  install prefix (default: $HOME/.flow); the binary is
#                 placed at $FLOW_INSTALL/bin/flow

set -e

repo_url="https://github.com/ibrahimthecosmic/flow"

if ! command -v unzip >/dev/null && ! command -v 7z >/dev/null; then
	echo "error: either unzip or 7z is required to install flow (see: https://github.com/denoland/deno_install#either-unzip-or-7z-is-required)." 1>&2
	exit 1
fi

os="$(uname -s)"
arch="$(uname -m)"

if [ "$os" != "Linux" ]; then
	echo "error: flow only ships Linux binaries; there is no build for $os." 1>&2
	echo "You can build from source instead: $repo_url#building-from-source" 1>&2
	exit 1
fi

if [ "$arch" != "x86_64" ]; then
	echo "error: flow only ships x86_64 binaries; there is no build for $arch." 1>&2
	echo "You can build from source instead: $repo_url#building-from-source" 1>&2
	exit 1
fi

# flow ships glibc binaries only; there is no musl build. On Alpine/musl,
# use a glibc-based container image (e.g. debian-slim) instead.
if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
	echo "error: flow requires glibc; musl-based systems (e.g. Alpine) are not supported." 1>&2
	echo "Use a glibc-based image such as debian:stable-slim, or build from source: $repo_url#building-from-source" 1>&2
	exit 1
elif [ -f /etc/alpine-release ]; then
	echo "error: flow requires glibc; Alpine Linux is not supported." 1>&2
	echo "Use a glibc-based image such as debian:stable-slim, or build from source: $repo_url#building-from-source" 1>&2
	exit 1
fi

target="x86_64-unknown-linux-gnu"

if [ $# -eq 0 ]; then
	flow_uri="${repo_url}/releases/latest/download/flow-${target}.zip"
else
	flow_uri="${repo_url}/releases/download/${1}/flow-${target}.zip"
fi

flow_install="${FLOW_INSTALL:-$HOME/.flow}"
bin_dir="$flow_install/bin"
exe="$bin_dir/flow"

if [ ! -d "$bin_dir" ]; then
	mkdir -p "$bin_dir"
fi

curl --fail --location --progress-bar --output "$exe.zip" "$flow_uri"
if command -v unzip >/dev/null; then
	unzip -d "$bin_dir" -o "$exe.zip"
else
	7z x -o"$bin_dir" -y "$exe.zip"
fi
chmod +x "$exe"
rm "$exe.zip"

echo "Flow was installed successfully to $exe"

if command -v flow >/dev/null; then
	echo "Run 'flow --help' to get started"
else
	case $SHELL in
	/bin/zsh) shell_profile=".zshrc" ;;
	*) shell_profile=".bashrc" ;;
	esac
	echo "Manually add the directory to your \$HOME/$shell_profile (or similar):"
	echo "  export FLOW_INSTALL=\"$flow_install\""
	echo "  export PATH=\"\$FLOW_INSTALL/bin:\$PATH\""
	echo "Run '$exe --help' to get started"
fi

echo
echo "Stuck? Open an issue at ${repo_url}/issues"
