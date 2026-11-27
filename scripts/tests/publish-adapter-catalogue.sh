#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
publisher=$root/scripts/publish-adapter-catalogue.sh
tool=${CATALOGUE_TOOL:-${CARGO_TARGET_DIR:-$root/target}/debug/examples/catalogue}
work=$(mktemp -d "${TMPDIR:-/tmp}/oxidepm-catalogue-test.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
export HOME="$work/home" OXIDEPM_HOME="$work/oxidepm"
export GIT_AUTHOR_NAME='Release tests' GIT_COMMITTER_NAME='Release tests'
export GIT_AUTHOR_EMAIL=release-test@example.invalid GIT_COMMITTER_EMAIL=release-test@example.invalid
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
mkdir -p "$HOME" "$OXIDEPM_HOME" "$work/bin" "$work/releases"
export CATALOGUE_TOOL="$tool" TEST_RELEASES="$work/releases" TEST_STATE="$work"
export PATH="$work/bin:$PATH"
repo=example/oxidepm
targets='x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu'
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
        --max-filesize|--proto|--proto-redir|--retry|--connect-timeout|--max-time) shift 2 ;;
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
    'api --method')
        test "$3" = PATCH && test "$4" = repos/example/oxidepm/releases/123
        printf '%s\n' "$@" | grep -Fxq 'draft=false'
        printf '%s\n' "$@" | grep -Fxq 'prerelease=false'
        printf '%s\n' "$@" | grep -Fxq 'make_latest=true'
        touch "$TEST_STATE/public"
        ;;
    'api repos/example/oxidepm/releases/123')
        jq -n --arg tag v1.0.0 '{draft:true, prerelease:false, tag_name:$tag}'
        ;;
    'api --paginate')
        test "$3" = --slurp && test "$4" = 'repos/example/oxidepm/releases/123/assets?per_page=100'
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
    special_name=${3-}
    special_version=${4-}
    bundles=$work/bundles
    rm -rf -- "$bundles"
    for target in $targets; do
        dir=$bundles/oxidepm-$target/adapters
        mkdir -p "$dir"
        printf 'core %s\n' "$target" > "$dir/../oxidepm-$target.tar.gz"
        printf 'bundle %s\n' "$target" > "$dir/../oxidepm-adapters-$target.tar.gz"
        for name in $names; do
            archive_version=$version
            if [ "$name" = "$special_name" ]; then archive_version=$special_version; fi
            printf '%s %s %s\n' "$name" "$target" "$archive_version" > "$dir/$name-$archive_version.tar.gz"
        done
        jq -n --arg target "$target" --arg tag "$tag" --arg version "$version" \
            --arg names "$names" --arg special "$special_name" --arg special_version "$special_version" '{
                schema:1, sequence:1, generated:"2025-01-01T00:00:00Z",
                expires:"2025-04-01T00:00:00Z", adapters:(
                    $names | split("\n") | map(. as $name | (if . == $special then $special_version else $version end) as $v | {
                        name:., kind:"service", summary:., releases:[{
                            version:$v, released:"2025-01-01", min_daemon:"0.1.0", protocol:[1],
                            artifacts:{($target): {
                                url:("https://github.com/example/oxidepm/releases/download/" + $tag + "/" + $name + "-" + $v + ".tar.gz"),
                                size:1, sha256:("0"*64), exe_sha256:("1"*64), manifest_sha256:("2"*64)
                            }}
                        }]
                    })
                )
            }' > "$dir/index.json"
        for name in $names; do
            archive_version=$version
            if [ "$name" = "$special_name" ]; then archive_version=$special_version; fi
            file=$dir/$name-$archive_version.tar.gz
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
}

edit_bundle_index() {
    dir=$work/bundles/oxidepm-x86_64-unknown-linux-gnu/adapters
    jq "$1" "$dir/index.json" > "$dir/next.json"
    mv "$dir/next.json" "$dir/index.json"
    sh "$root/scripts/bundle-checksums.sh" "$dir"
}

make_release v1.0.0 1.0.0
mv "$work/bundles/oxidepm-aarch64-unknown-linux-gnu" "$work/missing-target"
reject 'missing target' prepare v1.0.0 "$work/releases/invalid-matrix"
mv "$work/missing-target" "$work/bundles/oxidepm-aarch64-unknown-linux-gnu"
printf 'ok - requires every matrix target\n'

missing=$work/bundles/oxidepm-x86_64-unknown-linux-gnu/adapters/check-1.0.0.tar.gz
mv "$missing" "$work/missing-archive"
reject 'missing asset' prepare v1.0.0 "$work/releases/missing-asset"
mv "$work/missing-archive" "$missing"
printf 'ok - requires every bundled archive\n'

collision_dir=$work/bundles/oxidepm-x86_64-unknown-linux-gnu/adapters
cp "$collision_dir/index.json" "$work/original.json"
edit_bundle_index '(.adapters[] | select(.name == "deploy") | .releases[0].artifacts[].url) |= sub("deploy-"; "check-")'
reject 'unexpected bundle archive URL' prepare v1.0.0 "$work/releases/collision"
cp "$work/original.json" "$collision_dir/index.json"
sh "$root/scripts/bundle-checksums.sh" "$collision_dir"

edit_bundle_index '(.adapters[0].releases[0].min_daemon)="9.0.0"'
reject 'release metadata disagrees' prepare v1.0.0 "$work/releases/bad-metadata"
cp "$work/original.json" "$collision_dir/index.json"
sh "$root/scripts/bundle-checksums.sh" "$collision_dir"

edit_bundle_index '(.adapters[0].releases[0].artifacts[].sha256)=("3"*64)'
reject 'archive checksum disagrees' prepare v1.0.0 "$work/releases/bad-hash"
cp "$work/original.json" "$collision_dir/index.json"
sh "$root/scripts/bundle-checksums.sh" "$collision_dir"

cp "$missing" "$work/original-archive"
printf 'different bytes' >> "$missing"
reject 'archive size or hash disagrees' prepare v1.0.0 "$work/releases/bad-bytes"
cp "$work/original-archive" "$missing"
printf 'ok - rejects colliding basenames and inconsistent metadata or bytes\n'

prepare v1.0.0 "$work/releases/v1.0.0"
sign_release v1.0.0
release=$work/releases/v1.0.0
test "$(wc -l < "$release/checksums.txt")" -eq 33
for target in $targets; do
    cmp "$work/bundles/oxidepm-$target/adapters/check-1.0.0.tar.gz" \
        "$release/oxidepm-adapter-check-$target.tar.gz"
done
jq 'del(.adapters[0].releases[0].artifacts["aarch64-unknown-linux-gnu"])' \
    "$release/$input" > "$work/incomplete-input.json"
reject 'missing target' "$tool" advance "$repo" v1.0.0 - "$work/incomplete-input.json" "$work/invalid.json"
printf 'ok - keeps each target archive under its own release asset name\n'

git init --bare --quiet "$work/remote.git"
remote=$work/remote.git
branch=refs/heads/catalogue

publish() {
    sh "$publisher" "$@"
}

head_commit() {
    git --git-dir="$remote" rev-parse "$branch"
}

get_pair() {
    git --git-dir="$remote" show "$branch:index.json" > "$work/live.json"
    git --git-dir="$remote" show "$branch:index.json.minisig" > "$work/live.json.minisig"
}

assert_sequence() {
    get_pair
    "$tool" verify_pair "$work/live.json" "$work/live.json.minisig" "$CATALOGUE_PUBLIC_KEY" "$(($1 - 1))" >/dev/null
}

reject 'requires explicit bootstrap' publish advance "$repo" "$remote" v1.0.0
test -z "$(git ls-remote --refs "$remote" "$branch")"

archive=oxidepm-adapter-check-x86_64-unknown-linux-gnu.tar.gz
mv "$release/$archive" "$work/hidden-archive"
reject 'HTTP 404' publish advance "$repo" "$remote" v1.0.0 --bootstrap
mv "$work/hidden-archive" "$release/$archive"
cp "$release/$archive" "$work/original-archive"
printf 'changed archive' >> "$release/$archive"
reject 'size or hash disagrees' publish advance "$repo" "$remote" v1.0.0 --bootstrap
mv "$work/original-archive" "$release/$archive"

cp "$release/checksums.txt.minisig" "$work/original.minisig"
minisign -S -s "$work/key.sec" -m "$release/checksums.txt" -x "$release/checksums.txt.minisig" \
    -t 'oxidepm release checksums tag=v9.0.0' </dev/null
reject 'unexpected trusted comment' publish advance "$repo" "$remote" v1.0.0 --bootstrap
mv "$work/original.minisig" "$release/checksums.txt.minisig"

cp "$release/checksums.txt" "$work/original.sums"
cp "$release/checksums.txt.minisig" "$work/original.minisig"
awk -v name="$archive" '$2 != name' "$work/original.sums" > "$release/checksums.txt"
minisign -S -s "$work/key.sec" -m "$release/checksums.txt" -t 'oxidepm release checksums tag=v1.0.0' </dev/null
reject 'missing signed checksum' publish advance "$repo" "$remote" v1.0.0 --bootstrap
mv "$work/original.sums" "$release/checksums.txt"
mv "$work/original.minisig" "$release/checksums.txt.minisig"
test -z "$(git ls-remote --refs "$remote" "$branch")"

publish advance "$repo" "$remote" v1.0.0 --bootstrap
assert_sequence 1
seed=$(head_commit)
publish advance "$repo" "$remote" v1.0.0
test "$(head_commit)" = "$seed"
publish renew "$repo" "$remote"
test "$(head_commit)" = "$seed"
printf 'ok - requires explicit bootstrap and validates public assets and release signatures\n'

make_release v1.1.0 1.0.0
changed=$work/bundles/oxidepm-x86_64-unknown-linux-gnu/adapters
printf 'updated bytes\n' > "$changed/check-1.0.0.tar.gz"
sum=$(sha256sum "$changed/check-1.0.0.tar.gz")
jq --arg sha "${sum%% *}" --argjson size "$(wc -c < "$changed/check-1.0.0.tar.gz")" \
    '(.adapters[] | select(.name == "check") | .releases[0].artifacts[]) |= (.sha256=$sha | .exe_sha256=$sha | .size=$size)' \
    "$changed/index.json" > "$changed/next.json"
mv "$changed/next.json" "$changed/index.json"
sh "$root/scripts/bundle-checksums.sh" "$changed"
prepare v1.1.0 "$work/releases/v1.1.0"
sign_release v1.1.0
reject 'changed at the same version' publish advance "$repo" "$remote" v1.1.0
test "$(head_commit)" = "$seed"
printf 'ok - rejects version reuse with different bytes\n'

make_release v0.9.0 0.9.0
prepare v0.9.0 "$work/releases/v0.9.0"

policy=$work/policy
git init --quiet "$policy"
git -C "$policy" fetch --quiet "$remote" "$branch"
git -C "$policy" checkout --quiet --detach FETCH_HEAD
policy_commit() {
    git -C "$policy" add -- index.json index.json.minisig
    git -C "$policy" commit --quiet -m 'Update catalogue'
    git -C "$policy" push --quiet "$remote" "HEAD:$branch"
}
sign_policy() {
    sequence=$(jq -r .sequence "$policy/index.json")
    generated=$(jq -r .generated "$policy/index.json")
    minisign -S -s "$work/key.sec" -m "$policy/index.json" \
        -t "index sequence=$sequence generated=$generated" </dev/null
}

# Published catalogues can contain targets absent from new release inputs.
legacy_target=x86_64-apple-darwin
for adapter in check deploy; do
    cp "$release/oxidepm-adapter-$adapter-x86_64-unknown-linux-gnu.tar.gz" \
        "$release/oxidepm-adapter-$adapter-$legacy_target.tar.gz"
done
jq --arg legacy "$legacy_target" --slurpfile older "$work/releases/v0.9.0/$input" '.sequence=2 | .generated="2025-01-01T00:00:00Z" | .expires="2025-04-01T00:00:00Z" |
    (.adapters[] | select(.name == "deploy") | .releases[0]) |= (.yanked=true | .yank_reason="Withdrawn") |
    (.adapters[] | select(.name == "deploy") | .releases) += ($older[0].adapters[] | select(.name == "deploy") | .releases) |
    (.adapters[] | select(.name == "check" or .name == "deploy") | .releases[0].artifacts) |=
        (.[$legacy] = (."x86_64-unknown-linux-gnu" | .url |= sub("x86_64-unknown-linux-gnu"; $legacy))) |
    .revoked=[{exe_sha256:(.adapters[] | select(.name == "init") | .releases[0].artifacts["x86_64-unknown-linux-gnu"].exe_sha256), reason:"Revoked"}]' \
    "$work/live.json" > "$policy/index.json"
sign_policy
policy_commit
old=$(head_commit)
reject 'less than 45 days remain' publish check-expiry "$remote"
mv "$release/$archive" "$work/hidden-archive"
reject 'HTTP 404' publish renew "$repo" "$remote"
mv "$work/hidden-archive" "$release/$archive"
test "$(head_commit)" = "$old"

cat > "$remote/hooks/pre-receive" <<'HOOK'
#!/bin/sh
exit 1
HOOK
chmod +x "$remote/hooks/pre-receive"
reject 'catalogue push was rejected' publish renew "$repo" "$remote"
test "$(head_commit)" = "$old"
rm -- "$remote/hooks/pre-receive"
printf 'ok - keeps the previous pair when the server rejects the push\n'

cat > "$work/compete" <<MOCK
#!/bin/sh
OXIDEPM_MINISIGN_KEY=\$(cat "$work/key.sec")
export OXIDEPM_MINISIGN_KEY
export GIT_COMMITTER_NAME="Concurrent publisher"
exec sh "$publisher" renew "$repo" "$remote"
MOCK
chmod +x "$work/compete"
touch "$work/race"
publish renew "$repo" "$remote" > "$work/race.log" 2>&1
grep -Fq 'Catalogue branch changed; checking again' "$work/race.log"
assert_sequence 3
test "$(git --git-dir="$remote" rev-list --count "$branch")" -eq 3
jq -S 'del(.sequence, .generated, .expires)' "$policy/index.json" > "$work/expected.json"
jq -S 'del(.sequence, .generated, .expires)' "$work/live.json" > "$work/renewed.json"
cmp "$work/expected.json" "$work/renewed.json"
publish check-expiry "$remote"
printf 'ok - retries a rejected concurrent renewal and preserves every offer, yank and revocation\n'

make_release v2.0.0 1.0.0 check 2.0.0
prepare v2.0.0 "$work/releases/v2.0.0"
sign_release v2.0.0
publish advance "$repo" "$remote" v2.0.0
assert_sequence 4
jq -e --arg legacy "$legacy_target" '.adapters[] | select(.name=="check") | .releases[0] |
    .version=="2.0.0" and (.artifacts | has($legacy) | not)' "$work/live.json" >/dev/null
jq -e --arg legacy "$legacy_target" '.adapters[] | select(.name=="deploy") | .releases[0] |
    .yanked and .yank_reason=="Withdrawn" and (.artifacts | has($legacy)) and
    all(.artifacts[].url; contains("/v1.0.0/"))' "$work/live.json" >/dev/null
jq -e '.revoked | length == 1' "$work/live.json" >/dev/null
new_head=$(head_commit)
reject 'older release tag' publish advance "$repo" "$remote" v1.0.0
test "$(head_commit)" = "$new_head"
printf 'ok - advances versions, keeps policy on unchanged records and rejects a late older tag\n'

# A valid old pair in a new commit is still a rollback.
git -C "$policy" fetch --quiet "$remote" "$branch"
git -C "$policy" checkout --quiet --detach FETCH_HEAD
git --git-dir="$remote" show "$seed:index.json" > "$policy/index.json"
git --git-dir="$remote" show "$seed:index.json.minisig" > "$policy/index.json.minisig"
policy_commit
reject 'does not follow' publish renew "$repo" "$remote"
printf 'ok - rejects reverted signed pairs in branch history\n'

cp "$work/live.json" "$work/mixed.json"
git --git-dir="$remote" show "$seed:index.json.minisig" > "$work/mixed.minisig"
reject 'signature does not match' "$tool" verify_pair "$work/mixed.json" "$work/mixed.minisig" "$CATALOGUE_PUBLIC_KEY" 3
printf 'ok - rejects mixed catalogue generations\n'

for adapter in check deploy; do
    rm -- "$release/oxidepm-adapter-$adapter-$legacy_target.tar.gz"
done

touch "$work/fail-upload"
reject 'Upload failed' publish release "$repo" v1.0.0 "$release"
test -f "$work/draft" && test ! -e "$work/public" || fail 'An upload failure must leave a draft'
rm -- "$work/fail-upload" "$work/draft"
(
    cd "$release"
    for file in *; do
        sum=$(sha256sum -- "$file")
        printf '%s %s %s\n' "$file" "$(wc -c < "$file")" "${sum%% *}"
    done
) | jq -Rn '[inputs | split(" ") | {name:.[0], size:(.[1]|tonumber), digest:("sha256:" + .[2]), state:"uploaded"}]' > "$work/assets.json"
cp "$work/assets.json" "$work/complete-assets.json"
jq '.[0].digest="sha256:invalid"' "$work/complete-assets.json" > "$work/assets.json"
reject 'uploaded assets do not match' publish release "$repo" v1.0.0 "$release"
test -f "$work/draft" && test ! -e "$work/public" || fail 'A digest failure must leave a draft'
rm -- "$work/draft"

jq '.[1:]' "$work/complete-assets.json" > "$work/assets.json"
reject 'uploaded assets do not match' publish release "$repo" v1.0.0 "$release"
test -f "$work/draft" && test ! -e "$work/public" || fail 'A missing asset must leave a draft'
rm -- "$work/draft"

mv "$work/complete-assets.json" "$work/assets.json"
publish release "$repo" v1.0.0 "$release"
test -f "$work/public"
printf 'ok - leaves a draft on failed upload or verification and publishes complete assets\n'
