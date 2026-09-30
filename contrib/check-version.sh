#!/bin/sh
# Checks that every version matches a release tag: contrib/check-version.sh v0.2.0
# The crates carry the full version (v0.2.0-rc.1 → 0.2.0-rc.1); the extension
# only takes numbers, so it carries 0.2.0.
set -eu
cd "$(dirname "$0")/.."
full=${1#v}
plain=${full%%-*}
status=0
check() {
    found=$(grep -m1 -E "$2" "$1" | sed -E 's/.*"([^"]+)".*/\1/')
    if [ "$found" != "$3" ]; then
        echo "$1: version is $found, the tag needs $3" >&2
        status=1
    fi
}
check Cargo.toml '^version *= *"' "$full"
check extension/manifest.json '"version" *:' "$plain"
[ "$status" = 0 ] && echo "all versions match $1"
exit "$status"
