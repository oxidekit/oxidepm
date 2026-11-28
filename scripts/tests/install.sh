#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/oxidepm-install-test.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
mkdir "$work/home" "$work/oxidepm"

cat > "$work/uname" <<'MOCK'
#!/bin/sh
[ "$#" -eq 1 ] && [ "$1" = -s ] || exit 97
printf '%s\n' Darwin
MOCK
chmod +x "$work/uname"

status=0
HOME="$work/home" OXIDEPM_HOME="$work/oxidepm" PATH="$work" \
    /bin/sh "$root/scripts/install.sh" > "$work/stdout" 2> "$work/stderr" || status=$?
printf '%s\n' 'OxidePM supports Linux only (x86_64 and aarch64).' > "$work/expected"
if [ "$status" -ne 1 ] || [ -s "$work/stdout" ] || ! cmp -s "$work/expected" "$work/stderr"; then
    printf 'Expected immediate refusal on non-Linux (exit 1); got exit %s\n' "$status" >&2
    cat "$work/stdout" "$work/stderr" >&2
    exit 1
fi
printf 'ok - refuses non-Linux before checking architecture or installing\n'

mkdir "$work/bin" "$work/release" "$work/files"
for binary in oxidepm oxidepmd oxidepm-sandbox; do
    printf '#!/bin/sh\nprintf "oxidepm 1.0.0\\n"\n' > "$work/files/$binary"
    chmod 700 "$work/files/$binary"
done
bundle=share/oxidepm/adapters
mkdir -p "$work/files/$bundle/nested"
printf '%s\n' '{}' > "$work/files/$bundle/index.json"
printf '%s\n' 'checksums' > "$work/files/$bundle/SHA256SUMS"
printf '%s\n' 'test bundle signature' > "$work/files/$bundle/SHA256SUMS.minisig"
printf '%s\n' 'archive' > "$work/files/$bundle/nested/adapter.tar.gz"
chmod 700 "$work/files/$bundle" "$work/files/$bundle/nested"
chmod 600 "$work/files/$bundle/index.json" "$work/files/$bundle/SHA256SUMS" "$work/files/$bundle/SHA256SUMS.minisig"
chmod 700 "$work/files/$bundle/nested/adapter.tar.gz"
archive=oxidepm-x86_64-unknown-linux-gnu.tar.gz
tar --owner=42424 --group=42425 -czf "$work/release/$archive" -C "$work/files" \
    oxidepm oxidepmd oxidepm-sandbox share
(cd "$work/release" && sha256sum "$archive" > checksums.txt)
printf '%s\n' 'untrusted comment: test signature' test 'trusted comment: test' test > "$work/release/checksums.txt.minisig"

export TEST_INSTALL_WORK="$work"
export TEST_REAL_ID=$(command -v id)
export TEST_REAL_INSTALL=$(command -v install)
export TEST_REAL_TAR=$(command -v tar)
cat > "$work/bin/uname" <<'MOCK'
#!/bin/sh
case "$1" in -s) echo Linux ;; -m) echo x86_64 ;; *) exit 97 ;; esac
MOCK
cat > "$work/bin/curl" <<'MOCK'
#!/bin/sh
set -eu
case "$1" in -fsSL) shift ;; *) exit 97 ;; esac
if [ "$1" = -w ]; then shift 2; fi
case "$1" in
    */v1.0.0/*) cp "$TEST_INSTALL_WORK/release/${1##*/}" "$3" ;;
    *) exit 97 ;;
esac
MOCK
cat > "$work/bin/minisign" <<'MOCK'
#!/bin/sh
echo 'oxidepm release checksums tag=v1.0.0'
MOCK
cat > "$work/bin/id" <<'MOCK'
#!/bin/sh
if [ "$TEST_INSTALL_MODE" = root ] && [ "$1" = -u ]; then
    echo 0
else
    exec "$TEST_REAL_ID" "$@"
fi
MOCK
cat > "$work/bin/tar" <<'MOCK'
#!/bin/sh
printf '%s\n' "$@" >> "$TEST_INSTALL_WORK/tar-args"
exec "$TEST_REAL_TAR" "$@"
MOCK
cat > "$work/bin/sudo" <<'MOCK'
#!/bin/sh
set -eu
printf 'sudo %s %s %s\n' "$1" "${2-}" "${3-}" >> "$TEST_INSTALL_WORK/install-args"
chmod u+w "$INSTALL_DIR"
exec "$@"
MOCK
cat > "$work/bin/install" <<'MOCK'
#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$TEST_INSTALL_WORK/install-args"
if [ "$("$TEST_REAL_ID" -u)" -ne 0 ] &&
    [ "$1 $2 ${3-} ${4-} ${5-} ${6-}" = '-m 0755 -o root -g root' ]; then
    shift 6
    set -- -m 0755 "$@"
fi
exec "$TEST_REAL_INSTALL" "$@"
MOCK
chmod +x "$work/bin/"*

for mode in user root sudo; do
    if [ "$mode" = sudo ] && [ "$(id -u)" -eq 0 ]; then
        continue
    fi
    prefix="$work/$mode/bin"
    mkdir -p "$prefix"
    if [ "$mode" = sudo ]; then chmod 500 "$prefix"; fi
    parent_mode=755
    if [ "$mode" = root ]; then
        parent_mode=750
        mkdir -p "$work/$mode/share/oxidepm"
        chmod "$parent_mode" "$work/$mode/share" "$work/$mode/share/oxidepm"
    fi
    : > "$work/tar-args"
    : > "$work/install-args"
    if ! (
        umask 077
        HOME="$work/home" OXIDEPM_HOME="$work/oxidepm" OXIDEPM_VERSION=v1.0.0 \
            INSTALL_DIR="$prefix" TEST_INSTALL_MODE="$mode" PATH="$work/bin:$prefix:$PATH" \
            sh "$root/scripts/install.sh"
    ) > "$work/install.log" 2>&1; then
        cat "$work/install.log" >&2
        exit 1
    fi
    grep -Fxq -- --no-same-owner "$work/tar-args" || {
        printf '%s\n' 'Expected extraction without archive ownership' >&2
        exit 1
    }
    for binary in oxidepm oxidepmd oxidepm-sandbox; do
        test "$(stat -c %a "$prefix/$binary")" = 755
    done
    for dir in share share/oxidepm "$bundle" "$bundle/nested"; do
        case "$dir" in
            share|share/oxidepm) dir_mode=$parent_mode ;;
            *) dir_mode=755 ;;
        esac
        test "$(stat -c '%a %u %g' "$work/$mode/$dir")" = "$dir_mode $(id -u) $(id -g)" || {
            printf 'Unexpected permissions or ownership on %s in %s install\n' "$dir" "$mode" >&2
            exit 1
        }
    done
    for file in index.json SHA256SUMS SHA256SUMS.minisig nested/adapter.tar.gz; do
        cmp -s "$work/files/$bundle/$file" "$work/$mode/$bundle/$file"
        test "$(stat -c '%a %u %g' "$work/$mode/$bundle/$file")" = "644 $(id -u) $(id -g)" || {
            printf 'Unexpected permissions or ownership on %s in %s install\n' "$file" "$mode" >&2
            exit 1
        }
    done
    if [ "$mode" = user ] && [ "$(id -u)" -ne 0 ]; then
        test "$(stat -c %u "$prefix/oxidepm")" = "$(id -u)"
        test "$(grep -c '^-m 0755 -- ' "$work/install-args")" -eq 2
    else
        test "$(grep -c '^-m 0755 -o root -g root -- ' "$work/install-args")" -eq 2
    fi
    if [ "$mode" = sudo ]; then
        test "$(grep -c '^sudo install -m 0755$' "$work/install-args")" -eq 2
        test "$(grep -c '^sudo install -d -m$' "$work/install-args")" -eq 2
        test "$(grep -c '^sudo install -m 0644$' "$work/install-args")" -eq 4
    fi
    printf 'ok - installs executable binaries and readable adapters as %s\n' "$mode"
done
