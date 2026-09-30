#!/bin/sh
# habitfocus installer and updater.
#
#   curl -fsSL https://github.com/TheYoyoyfreak/habitfocus/releases/latest/download/install.sh | sh
#
# Installs habitd and hf into ~/.local/bin, then runs `hf setup` (config,
# systemd user service, browser host). Run again to update; `hf update` does that.
#
# Options (after `sh -s --` when piped):
#   --version vX.Y.Z   install that release instead of the latest
#   --prefix DIR       where the binaries go (default ~/.local/bin)
#   --from FILE        install a downloaded habitfocus-<arch>-linux.tar.gz
#   --uninstall        stop and remove habitd, hf and the service (config and data stay)
set -eu

REPO=${HABITFOCUS_REPO:-TheYoyoyfreak/habitfocus}
version=latest
prefix=""
from=""
uninstall=0

say() { printf '%s\n' "$*"; }
usage() {
    say "habitfocus installer: install.sh [--version vX.Y.Z] [--prefix DIR] [--from FILE] [--uninstall]"
    say "  --version   install that release instead of the latest"
    say "  --prefix    where habitd and hf go (default ~/.local/bin)"
    say "  --from      install a downloaded habitfocus-<arch>-linux.tar.gz"
    say "  --uninstall stop and remove habitd, hf and the service (config and data stay)"
}
die() { printf 'habitfocus: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --version) version=${2:?--version needs a tag like v0.2.0}; shift 2 ;;
        --prefix) prefix=${2:?--prefix needs a directory}; shift 2 ;;
        --from) from=${2:?--from needs a file}; shift 2 ;;
        --uninstall) uninstall=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown option $1 (see --help)" ;;
    esac
done

data="${XDG_DATA_HOME:-$HOME/.local/share}/habitfocus"
unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/habitd.service"
# An update goes where the last install went.
if [ -z "$prefix" ]; then
    prefix=$(cat "$data/installed-by" 2>/dev/null || true)
    prefix=${prefix:-$HOME/.local/bin}
fi

if [ "$uninstall" = 1 ]; then
    if [ -x "$prefix/hf" ]; then "$prefix/hf" setup --uninstall; fi
    rm -f "$prefix/hf" "$prefix/habitd"
    rm -rf "$data/installed-by"
    say "Removed habitd and hf from $prefix."
    exit 0
fi

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "no release for $(uname -m); build from source: https://github.com/$REPO#from-source" ;;
esac
asset="habitfocus-$arch-linux.tar.gz"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$2" "$1"
    else
        die "needs curl or wget"
    fi
}

if [ -n "$from" ]; then
    cp "$from" "$tmp/$asset"
    say "Installing $from"
else
    if [ "$version" = latest ]; then
        base="https://github.com/$REPO/releases/latest/download"
    else
        base="https://github.com/$REPO/releases/download/$version"
    fi
    say "Downloading $asset ($version)"
    fetch "$base/$asset" "$tmp/$asset" || die "download failed: $base/$asset"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
    (cd "$tmp" && grep " $asset\$" SHA256SUMS | sha256sum -c - >/dev/null) || die "checksum mismatch for $asset"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
src="$tmp/habitfocus-$arch-linux"
[ -x "$src/hf" ] && [ -x "$src/habitd" ] || die "$asset doesn't contain hf and habitd"

updating=0
[ -f "$unit" ] && updating=1

mkdir -p "$prefix" "$data"
for bin in habitd hf; do
    # Copy, then rename: replacing a running binary is fine that way.
    cp "$src/$bin" "$prefix/.$bin.new"
    chmod 755 "$prefix/.$bin.new"
    mv -f "$prefix/.$bin.new" "$prefix/$bin"
done
printf '%s\n' "$prefix" >"$data/installed-by"
say "Installed $("$prefix/hf" --version) into $prefix"

# An update restarts a running daemon; one the user hasn't started stays off.
if [ "$updating" = 1 ] && [ -z "${HABITFOCUS_NO_SYSTEMCTL:-}" ] &&
    systemctl --user is-active --quiet habitd 2>/dev/null; then
    "$prefix/hf" setup --start
else
    "$prefix/hf" setup
fi

case ":$PATH:" in
    *":$prefix:"*) ;;
    *)
        say ""
        say "$prefix isn't on your PATH. Add this to ~/.bashrc (or your shell's config):"
        say "  export PATH=\"$prefix:\$PATH\""
        ;;
esac
if command -v noctalia >/dev/null 2>&1 && [ "$updating" = 0 ]; then
    say ""
    say "Noctalia plugin: https://github.com/TheYoyoyfreak/habitfocus_noctalia_plugin"
fi
