#!/bin/sh
set -eu

cd "${1:?usage: bundle-checksums.sh BUNDLE_DIR}"
set -- ./*.tar.gz
test -f "$1" || { echo "The adapter bundle has no archives" >&2; exit 1; }

sums=$(mktemp .SHA256SUMS.XXXXXX)
trap 'rm -f "$sums"' EXIT
for file in index.json "$@"; do
    name=${file#./}
    test -f "$name" && test ! -L "$name" || { echo "Missing regular bundle file: $name" >&2; exit 1; }
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "$name"
    else
        shasum -a 256 -- "$name"
    fi
done > "$sums"
chmod 644 "$sums"
mv "$sums" SHA256SUMS
