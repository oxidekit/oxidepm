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
