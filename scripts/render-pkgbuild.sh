#!/bin/sh
set -eu

tag=${1:?usage: render-pkgbuild.sh RELEASE_TAG CHECKSUMS}
checksums=${2:?missing release checksums}
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
case "$tag" in
    *[!A-Za-z0-9.+-]*)
        echo "Invalid release tag: $tag" >&2
        exit 1
        ;;
    v[0-9]*) ;;
    *)
        echo "Invalid release tag: $tag" >&2
        exit 1
        ;;
esac

checksum() {
    LC_ALL=C awk -v name="$1" '
        $2 == name { count++; hash = $1 }
        END {
            if (count != 1 || length(hash) != 64 || hash ~ /[^0-9a-f]/) exit 1
            print hash
        }
    ' "$checksums" || {
        printf 'Expected one SHA-256 checksum for %s\n' "$1" >&2
        return 1
    }
}

source=$(checksum oxidepm-source.tar.gz)
x86_64=$(checksum oxidepm-adapters-x86_64-unknown-linux-gnu.tar.gz)
aarch64=$(checksum oxidepm-adapters-aarch64-unknown-linux-gnu.tar.gz)
sed -e "s/@RELEASE_TAG@/$tag/" -e "s/@SOURCE_SHA256@/$source/" \
    -e "s/@X86_64_SHA256@/$x86_64/" -e "s/@AARCH64_SHA256@/$aarch64/" "$root/dist/aur/PKGBUILD"
