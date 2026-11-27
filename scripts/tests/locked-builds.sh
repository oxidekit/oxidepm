#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$root"
git ls-files --error-unmatch Cargo.lock >/dev/null
awk '
    /cargo (build|clippy|test)([[:space:]]|$)/ && !/(^|[[:space:]])--locked([[:space:]]|$)/ {
        print FILENAME ":" FNR ": Cargo must use the checked-in lockfile"
        invalid = 1
    }
    END { exit invalid }
' .github/workflows/*.yml Dockerfile dist/aur/PKGBUILD
printf '%s\n' 'ok - workflows, Dockerfile and PKGBUILD use the checked-in lockfile'
