#!/bin/sh
set -eu

version=${1:?usage: deb-version.sh RELEASE_TAG}
version=${version#v}
case "$version" in
    *-*) printf '%s~%s\n' "${version%%-*}" "${version#*-}" ;;
    *) printf '%s\n' "$version" ;;
esac
