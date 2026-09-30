#!/bin/sh
# Packages release binaries into dist/habitfocus-<arch>-linux.tar.gz, the
# archive install.sh installs.
#
#   contrib/package-release.sh x86_64-unknown-linux-musl [binary dir]
#
# The binary dir defaults to target/<target>/release.
set -eu
cd "$(dirname "$0")/.."
target=${1:?usage: contrib/package-release.sh <rust target> [binary dir]}
bin=${2:-target/$target/release}
name="habitfocus-${target%%-*}-linux"
out="dist/$name"

rm -rf "$out"
mkdir -p "$out"
cp "$bin/habitd" "$bin/hf" "$out/"
cp contrib/install.sh contrib/habitd.service contrib/config.example.toml README.md LICENSE "$out/"
tar -C dist -czf "dist/$name.tar.gz" "$name"
echo "wrote dist/$name.tar.gz"
