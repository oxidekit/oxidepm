#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
publisher=$root/scripts/publish-adapter-catalogue.sh
tool=${CATALOGUE_TOOL:-${CARGO_TARGET_DIR:-$root/target}/debug/examples/catalogue}
work=$(mktemp -d "${TMPDIR:-/mnt/storage}/oxidepm-catalogue-test.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
export HOME="$work/home" OXIDEPM_HOME="$work/oxidepm"
export GIT_AUTHOR_NAME='Release tests' GIT_COMMITTER_NAME='Release tests'
export GIT_AUTHOR_EMAIL=release-test@example.invalid GIT_COMMITTER_EMAIL=release-test@example.invalid
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
mkdir -p "$HOME" "$OXIDEPM_HOME" "$work/bin" "$work/releases"
export CATALOGUE_TOOL="$tool" TEST_RELEASES="$work/releases" TEST_STATE="$work"
export PATH="$work/bin:$PATH"
repo=example/oxidepm
targets='x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin'
input=adapter-catalogue-input.json

fail() {
    echo "$*" >&2
    exit 1
}

reject() {
    message=$1
    shift
    if "$@" > "$work/rejected.log" 2>&1; then
        fail "Command unexpectedly succeeded: $*"
    fi
    if ! grep -Fq -- "$message" "$work/rejected.log"; then
        cat "$work/rejected.log" >&2
        fail "Expected an error containing: $message"
    fi
}

minisign -G -W -p "$work/key.pub" -s "$work/key.sec" >/dev/null
export CATALOGUE_PUBLIC_KEY=$(tail -n 1 "$work/key.pub")
export OXIDEPM_MINISIGN_KEY=$(cat "$work/key.sec")

cat > "$work/bin/curl" <<'MOCK'
#!/bin/sh
set -eu
while [ "$#" -gt 1 ]; do
    case $1 in
        --output) out=$2; shift 2 ;;
        --max-filesize|--proto|--proto-redir|--retry) shift 2 ;;
        --*) shift ;;
        *) exit 2 ;;
    esac
done
case $1 in
    https://github.com/example/oxidepm/releases/download/*)
        path=${1#https://github.com/example/oxidepm/releases/download/}
        test -f "$TEST_RELEASES/$path" || { echo "HTTP 404: $1" >&2; exit 22; }
        cp -- "$TEST_RELEASES/$path" "$out"
        ;;
    *) exit 2 ;;
esac
MOCK

cat > "$work/bin/gh" <<'MOCK'
#!/bin/sh
set -eu
case "$1 $2" in
    'release create')
        shift 2
        test ! -e "$TEST_STATE/draft"
        printf '%s\n' "$@" | grep -Fxq -- --draft
        touch "$TEST_STATE/draft"
        if [ -e "$TEST_STATE/fail-upload" ]; then
            echo 'Upload failed' >&2
            exit 1
        fi
        ;;
    'release view')
        test -e "$TEST_STATE/draft"
        printf '123\n'
        ;;
    'api repos/example/oxidepm/releases/123')
        if [ "$#" -gt 2 ]; then
            printf '%s\n' "$@" | grep -Fxq 'draft=false'
            printf '%s\n' "$@" | grep -Fxq 'prerelease=false'
            printf '%s\n' "$@" | grep -Fxq 'make_latest=true'
            touch "$TEST_STATE/public"
        else
            jq -n --arg tag v1.0.0 '{draft:true, prerelease:false, tag_name:$tag}'
        fi
        ;;
    'api repos/example/oxidepm/releases/123/assets?per_page=100')
        jq -n --slurpfile assets "$TEST_STATE/assets.json" '[$assets[0]]'
        ;;
    *) exit 2 ;;
esac
MOCK

real_git=$(command -v git)
export REAL_GIT="$real_git"
cat > "$work/bin/git" <<'MOCK'
#!/bin/sh
set -eu
if [ -e "$TEST_STATE/race" ]; then
    for arg do
        if [ "$arg" = push ]; then
            rm -- "$TEST_STATE/race"
            "$TEST_STATE/compete"
            break
        fi
    done
fi
exec "$REAL_GIT" "$@"
MOCK
chmod +x "$work/bin/"*

names=$(bash "$root/scripts/build-adapter-bundle.sh" --list)
make_release() {
    tag=$1
    version=$2
    bundles=$work/bundles
    rm -rf -- "$bundles"
    for target in $targets; do
        dir=$bundles/oxidepm-$target/adapters
        mkdir -p "$dir"
        printf 'core %s\n' "$target" > "$dir/../oxidepm-$target.tar.gz"
        printf 'bundle %s\n' "$target" > "$dir/../oxidepm-adapters-$target.tar.gz"
        for name in $names; do
            printf '%s %s %s\n' "$name" "$target" "$version" > "$dir/$name-$version.tar.gz"
        done
        jq -n --arg target "$target" --arg tag "$tag" --arg version "$version" \
            --arg names "$names" '{
                schema:1, sequence:1, generated:"2025-01-01T00:00:00Z",
                expires:"2025-04-01T00:00:00Z", adapters:(
                    $names | split("\n") | map(. as $name | {
                        name:., kind:"service", summary:., releases:[{
                            version:$version, released:"2025-01-01", min_daemon:"0.1.0", protocol:[1],
                            artifacts:{($target): {
                                url:("https://github.com/example/oxidepm/releases/download/" + $tag + "/" + $name + "-" + $version + ".tar.gz"),
                                size:1, sha256:("0"*64), exe_sha256:("1"*64), manifest_sha256:("2"*64)
                            }}
                        }]
                    })
                )
            }' > "$dir/index.json"
        for name in $names; do
            file=$dir/$name-$version.tar.gz
            sum=$(sha256sum "$file")
            jq --arg name "$name" --arg target "$target" --arg sha "${sum%% *}" \
                --argjson size "$(wc -c < "$file")" '(.adapters[] | select(.name == $name) |
                    .releases[0].artifacts[$target]) |= (.sha256=$sha | .size=$size | .exe_sha256=$sha)' \
                "$dir/index.json" > "$dir/next.json"
            mv "$dir/next.json" "$dir/index.json"
        done
        sh "$root/scripts/bundle-checksums.sh" "$dir"
    done
    deb_version=$(printf '%s' "${tag#v}" | tr - .)
    printf 'amd64\n' > "$bundles/oxidepm-x86_64-unknown-linux-gnu/oxidepm_${deb_version}_amd64.deb"
    printf 'arm64\n' > "$bundles/oxidepm-aarch64-unknown-linux-gnu/oxidepm_${deb_version}_arm64.deb"
}

prepare() {
    # The list is also used to build the offline bundles.
    # shellcheck disable=SC2086
    "$tool" prepare "$repo" "$1" 2025-01-01 "$work/bundles" "$2" $names
}

sign_release() {
    dir=$work/releases/$1
    (cd "$dir" && sha256sum -- ./*.tar.gz ./*.deb "$input" | sed 's/  \.\//  /' > checksums.txt)
    minisign -S -s "$work/key.sec" -m "$dir/checksums.txt" -t "oxidepm release checksums tag=$1" </dev/null
    printf 'formula\n' > "$dir/oxidepm.rb"
}

edit_bundle_index() {
    dir=$work/bundles/oxidepm-x86_64-apple-darwin/adapters
    jq "$1" "$dir/index.json" > "$dir/next.json"
    mv "$dir/next.json" "$dir/index.json"
    sh "$root/scripts/bundle-checksums.sh" "$dir"
}

make_release v1.0.0 1.0.0
mv "$work/bundles/oxidepm-aarch64-apple-darwin" "$work/missing-target"
reject 'missing target' prepare v1.0.0 "$work/releases/invalid-matrix"
