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
' .github/workflows/ci.yml .github/workflows/release.yml
printf '%s\n' 'ok - CI and release builds use the checked-in lockfile'
