#!/bin/sh
set +x
set -eu
umask 077

fail() {
    echo "Catalogue publication: $*" >&2
    exit 1
}

work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/oxidepm-catalogue.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
trap 'exit 1' HUP INT TERM
tool=${CATALOGUE_TOOL:-target/debug/examples/catalogue}
public_key=${CATALOGUE_PUBLIC_KEY:--}
branch=refs/heads/catalogue
input=adapter-catalogue-input.json

publish_release() {
    release_repo=$1
    release_tag=$2
    assets_dir=$3
    # Resolve the draft by tag once and use its ID for verification and publication.
    if gh release view "$release_tag" --repo "$release_repo" --json databaseId >/dev/null 2>&1; then
        fail "a release already exists for $release_tag"
    fi

    set -- "$assets_dir/"*
    for file in checksums.txt checksums.txt.minisig; do
        test -s "$assets_dir/$file" || fail "missing release asset $file"
    done
    "$tool" verify_checksums "$assets_dir/checksums.txt" "$assets_dir/checksums.txt.minisig" \
        "$public_key" "$release_tag" > "$work/checksums"
    for file do
        test -f "$file" && test -s "$file" || fail "missing release asset $file"
        hash=$(sha256sum -- "$file")
        printf '%s %s %s\n' "${file##*/}" "$(wc -c < "$file")" "${hash%% *}"
    done > "$work/expected"
    awk '$1 !~ /^(checksums[.]txt([.]minisig)?)$/ { print $3 "  " $1 }' "$work/expected" |
        LC_ALL=C sort -k2 > "$work/local-checksums"
    cmp -s "$work/checksums" "$work/local-checksums" || fail "release files disagree with signed checksums"

    prerelease=false
    make_latest=true
    # In a SemVer tag, only a hyphen before any build metadata marks a pre-release.
    case "${release_tag%%+*}" in
        *-*)
            prerelease=true
            make_latest=false
            set -- --prerelease "$@"
            ;;
    esac
    gh release create "$release_tag" --repo "$release_repo" --verify-tag --draft --generate-notes "$@"
    id=$(gh release view "$release_tag" --repo "$release_repo" --json databaseId --jq .databaseId)
    case "$id" in ''|*[!0-9]*) fail "invalid release ID" ;; esac
    endpoint="repos/$release_repo/releases/$id"

    gh api "$endpoint" > "$work/draft"
    jq -e --arg tag "$release_tag" --argjson prerelease "$prerelease" \
        '.draft and (.prerelease == $prerelease) and (.tag_name == $tag)' "$work/draft" >/dev/null ||
        fail "expected a draft for $release_tag"
    gh api --paginate --slurp "$endpoint/assets?per_page=100" > "$work/uploaded"
    jq -e --rawfile expected "$work/expected" '
        ($expected | split("\n") | map(select(length > 0) | split(" ") |
            {name:.[0], size:(.[1]|tonumber), digest:("sha256:" + .[2]), state:"uploaded"}) | sort_by(.name)) ==
        (flatten | map({name, size, digest, state}) | sort_by(.name))
    ' "$work/uploaded" >/dev/null || fail "uploaded assets do not match the release files"

    gh api --method PATCH "$endpoint" -F draft=false -F "prerelease=$prerelease" -f "make_latest=$make_latest" --silent
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
        printf 'prerelease=%s\n' "$prerelease" >> "$GITHUB_OUTPUT"
    fi
}

download() {
    curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
        --retry 2 --connect-timeout 15 --max-time 180 --max-filesize "$2" --output "$3" "$1"
    test -s "$3" || fail "empty release asset $1"
}

listed_hash() {
    awk -v name="$1" '$2 == name { print $1; found=1 } END { if (!found) exit 1 }' "$work/checksums"
}

check_assets() {
    "$tool" assets "$repo" "$1" > "$work/assets"
    while read -r digest size url; do
        if [ "$2" = release ]; then
            listed=$(listed_hash "${url##*/}") || fail "missing signed checksum for $url"
            [ "$listed" = "$digest" ] || fail "signed checksum disagrees with the catalogue for $url"
        fi
        record="$digest $size $url"
        if grep -Fxq -- "$record" "$work/verified-assets"; then
            continue
        fi
        download "$url" "$size" "$work/asset"
        hash=$(sha256sum -- "$work/asset")
        if [ "${hash%% *}" != "$digest" ] || [ "$(wc -c < "$work/asset")" -ne "$size" ]; then
            fail "archive size or hash disagrees for $url"
        fi
        printf '%s\n' "$record" >> "$work/verified-assets"
    done < "$work/assets"
}

release_input() {
    base="https://github.com/$repo/releases/download/$tag"
    download "$base/checksums.txt" 65536 "$work/checksums.txt"
    download "$base/checksums.txt.minisig" 4096 "$work/checksums.txt.minisig"
    "$tool" verify_checksums "$work/checksums.txt" "$work/checksums.txt.minisig" "$public_key" "$tag" > "$work/checksums"
    expected=$(listed_hash "$input") || fail "missing signed checksum for $input"
    download "$base/$input" 2097152 "$work/$input"
    hash=$(sha256sum -- "$work/$input")
    [ "${hash%% *}" = "$expected" ] || fail "signed checksum does not match $input"
}

git_in() {
    git -C "$work/repository" "$@"
}

remote_head() {
    if refs=$(git_in ls-remote --exit-code --refs "$remote" "$branch"); then
        printf '%s\n' "$refs" | cut -f 1
    else
        result=$?
        [ "$result" -eq 2 ] || return "$result"
    fi
}

verify_history() {
    merges=$(git_in rev-list --min-parents=2 "$head")
    [ -z "$merges" ] || fail "catalogue history must be linear"
    git_in rev-list --reverse "$head" > "$work/history"
    sequence=0
    while IFS= read -r revision; do
        files=$(git_in ls-tree --format='%(objectmode) %(path)' "$revision")
        [ "$files" = "$(printf '100644 index.json\n100644 index.json.minisig')" ] ||
            fail "catalogue commit $revision must contain a signed pair"
        git_in show "$revision:index.json" > "$work/current.json"
        git_in show "$revision:index.json.minisig" > "$work/current.json.minisig"
        sequence=$("$tool" verify_pair "$work/current.json" "$work/current.json.minisig" "$public_key" "$sequence") ||
            fail "could not verify catalogue commit $revision"
    done < "$work/history"
}

write_push_hook() {
    mkdir "$work/hooks"
    # A normal push checks for changes after receiving the remote refs. Check the
    # advertised head too, including when a branch was deleted before the push.
    cat > "$work/hooks/pre-push" <<'HOOK'
#!/bin/sh
set -eu
while read -r local_ref local_oid remote_ref remote_oid; do
    if [ "$remote_ref" != refs/heads/catalogue ] || [ "$remote_oid" != "$CATALOGUE_EXPECTED_HEAD" ]; then
        echo 'The catalogue branch tip changed before push' >&2
        exit 1
    fi
done
HOOK
    chmod 700 "$work/hooks/pre-push"
}

run_catalogue() {
    git init --quiet "$work/repository"
    write_push_hook
    touch "$work/verified-assets"
    last_head=
    attempt=1
    while [ "$attempt" -le 3 ]; do
        head=$(remote_head)
        current=-
        sequence=0
        if [ -n "$head" ]; then
            git_in fetch --quiet --no-tags "$remote" "$branch"
            head=$(git_in rev-parse FETCH_HEAD)
            if [ -n "$last_head" ] && ! git_in merge-base --is-ancestor "$last_head" "$head"; then
                fail "catalogue history was rewritten"
            fi
            verify_history
            current=$work/current.json
        elif [ "$bootstrap" != true ] || [ -n "$last_head" ]; then
            fail "the catalogue branch is missing and requires explicit bootstrap with a published tag"
        fi

        rm -f -- "$work/index.json" "$work/index.json.minisig"
        case "$mode" in
            check-expiry)
                "$tool" check_expiry "$current"
                return
                ;;
            advance)
                if [ ! -f "$work/$input" ]; then
                    release_input
                fi
                comment=$("$tool" advance "$repo" "$tag" "$current" "$work/$input" "$work/index.json")
                check_assets "$work/$input" release
                ;;
            renew)
                comment=$("$tool" renew "$current" "$work/index.json")
                ;;
        esac
        if [ ! -f "$work/index.json" ]; then
            echo 'Catalogue is current'
            return
        fi

        check_assets "$work/index.json" catalogue
        minisign -S -s "$work/minisign.key" -m "$work/index.json" -t "$comment" </dev/null
        next=$("$tool" verify_pair "$work/index.json" "$work/index.json.minisig" "$public_key" "$sequence")
        (
            cd "$work/repository"
            cp "$work/index.json" "$work/index.json.minisig" .
            git add -- index.json index.json.minisig
        )
        tree=$(git_in write-tree)
        if [ -n "$head" ]; then
            commit=$(git_in commit-tree "$tree" -p "$head" -m "Publish adapter catalogue sequence $next")
        else
            commit=$(git_in commit-tree "$tree" -m "Publish adapter catalogue sequence $next")
        fi
        # The hook expects an all-zero object name during explicit bootstrap.
        CATALOGUE_EXPECTED_HEAD=${head:-$(printf '%s' "$commit" | tr '0-9a-f' '0')}
        export CATALOGUE_EXPECTED_HEAD
        if OXIDEPM_CATALOGUE_HOOKS="$work/hooks" git_in --config-env=core.hooksPath=OXIDEPM_CATALOGUE_HOOKS \
            push "$remote" "$commit:$branch"; then
            return
        fi
        observed=$(remote_head)
        if [ "$observed" = "$head" ]; then
            fail "catalogue push was rejected"
        fi
        echo 'Catalogue branch changed; checking again' >&2
        last_head=$head
        attempt=$((attempt + 1))
    done
    fail "catalogue branch kept changing"
}

mode=${1-}
bootstrap=false
case "$mode:$#" in
    release:4)
        publish_release "$2" "$3" "$4"
        exit
        ;;
    check-expiry:2)
        remote=$2
        ;;
    renew:3)
        repo=$2
        remote=$3
        ;;
    advance:4|advance:5)
        repo=$2
        remote=$3
        tag=$4
        if [ "$#" -eq 5 ]; then
            [ "$5" = --bootstrap ] || fail "expected --bootstrap"
            bootstrap=true
        fi
        case "$tag" in
            v[0-9]*) case "$tag" in *[!a-zA-Z0-9.+-]*) fail "invalid release tag" ;; esac ;;
            *) fail "expected a version tag beginning with v" ;;
        esac
        ;;
    *)
        fail "usage: $0 release REPO TAG ASSET_DIR | advance REPO REMOTE TAG [--bootstrap] | renew REPO REMOTE | check-expiry REMOTE"
        ;;
esac

if [ "$mode" != check-expiry ]; then
    : "${OXIDEPM_MINISIGN_KEY:?Missing release signing key}"
    printf '%s\n' "$OXIDEPM_MINISIGN_KEY" > "$work/minisign.key"
    unset OXIDEPM_MINISIGN_KEY
fi
run_catalogue
