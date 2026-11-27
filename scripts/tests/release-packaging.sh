#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/oxidepm-packaging-test.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
mkdir "$work/assets" "$work/bin" "$work/bundle"
tag=v1.2.3-rc.1

test_pkgbuild() {
    if grep -Fq "'SKIP'" "$root/dist/aur/PKGBUILD"; then
        echo 'The package template skips source verification' >&2
        exit 1
    fi
    (
        cd "$work/assets"
        for asset in oxidepm-source.tar.gz oxidepm-adapters-x86_64-unknown-linux-gnu.tar.gz \
            oxidepm-adapters-aarch64-unknown-linux-gnu.tar.gz; do
            printf '%s\n' "$asset" > "$asset"
        done
        sha256sum -- *.tar.gz > checksums.txt
    )
    sh "$root/scripts/render-pkgbuild.sh" "$tag" "$work/assets/checksums.txt" > "$work/PKGBUILD"
    for arch in x86_64 aarch64; do
        CARCH="$arch" bash -eu -c '
            source "$1"
            test "$_tag" = "$2"
            test "$pkgver" = 1.2.3rc.1
            arch_sources="source_$CARCH[@]"
            arch_sums="sha256sums_$CARCH[@]"
            urls=("${source[@]}" "${!arch_sources}")
            hashes=("${sha256sums[@]}" "${!arch_sums}")
            test "${#urls[@]}" -eq 2
            for i in "${!urls[@]}"; do
                download_url=${urls[i]#*::}
                test "$download_url" = "$url/releases/download/$_tag/${download_url##*/}"
                printf "%s  %s\n" "${hashes[i]}" "${download_url##*/}"
            done | (cd "$3" && sha256sum --strict -c -)
        ' _ "$work/PKGBUILD" "$tag" "$work/assets"
    done
    printf 'ok - renders exact release checksums for both package architectures\n'

    for bad in missing duplicate malformed; do
        case "$bad" in
            missing) sed '/  oxidepm-source\.tar\.gz$/d' "$work/assets/checksums.txt" ;;
            duplicate) cat "$work/assets/checksums.txt" "$work/assets/checksums.txt" ;;
            malformed) sed 's/^[0-9a-f]*  oxidepm-source\.tar\.gz$/invalid  oxidepm-source.tar.gz/' "$work/assets/checksums.txt" ;;
        esac > "$work/bad-checksums"
        if sh "$root/scripts/render-pkgbuild.sh" "$tag" "$work/bad-checksums" > "$work/bad-PKGBUILD"; then
            echo "Rendered a PKGBUILD with a $bad checksum" >&2
            exit 1
        fi
        test ! -s "$work/bad-PKGBUILD"
    done
    printf 'ok - refuses missing, duplicate, and malformed package checksums\n'
}

test_docker_bundle() {
    archive=oxidepm-adapters-x86_64-unknown-linux-gnu.tar.gz
    printf '{}\n' > "$work/bundle/index.json"
    (cd "$work/bundle" && sha256sum index.json > SHA256SUMS)
    tar -czf "$work/assets/$archive" -C "$work/bundle" .
    (cd "$work/assets" && sha256sum "$archive" > checksums.txt)
    cp "$work/assets/checksums.txt" "$work/original-checksums"
    export TEST_PACKAGING_ASSETS="$work/assets"
    export TEST_PACKAGING_MINISIGN=$(command -v minisign)
    minisign -G -W -p "$work/key.pub" -s "$work/key.sec" >/dev/null
    export TEST_PACKAGING_KEY=$(tail -n 1 "$work/key.pub")
    cat > "$work/bin/minisign" <<'MOCK'
#!/bin/sh
set -eu
test "$1 $2 $3 $4" = '-V -H -Q -P'
test "$5" = 'RWQMeVEZKn2/BHzgJLldWbECE2BqKV4a0rmNXEzn5N0lxrSi/dDuFjHC'
shift 5
exec "$TEST_PACKAGING_MINISIGN" -V -H -Q -P "$TEST_PACKAGING_KEY" "$@"
MOCK
    cat > "$work/bin/curl" <<'MOCK'
#!/bin/sh
set -eu
test "$1" = -fsSL
case "$2" in https://example.invalid/download/*/*) ;; *) exit 97 ;; esac
if [ "$#" -eq 2 ]; then
    cat "$TEST_PACKAGING_ASSETS/${2##*/}"
else
    test "$#" -eq 4 && test "$3" = -o
    cp "$TEST_PACKAGING_ASSETS/${2##*/}" "$4"
fi
MOCK
    cat > "$work/bin/dpkg" <<'MOCK'
#!/bin/sh
test "$1" = --print-architecture && echo amd64
MOCK
    chmod +x "$work/bin/"*

    # Exercise the image's shell command with local release fixtures.
    sed -n '/^RUN set -eu/,/^$/p' "$root/Dockerfile" |
        sed -e '1s/^RUN //' -e "s|/usr/share/oxidepm/adapters|$work/extracted|g" > "$work/docker-run"
    test -s "$work/docker-run"

    sign_checksums() {
        minisign -S -s "$work/key.sec" -m "$work/assets/checksums.txt" \
            -x "$work/assets/checksums.txt.minisig" -t "oxidepm release checksums tag=$1" </dev/null
    }

    sign_checksums "$tag"
    ADAPTER_BUNDLE_URL="https://example.invalid/download/$tag" PATH="$work/bin:$PATH" sh "$work/docker-run"
    test -f "$work/extracted/index.json"
    printf 'ok - extracts an adapter bundle with authenticated release checksums\n'

    for rejected in signature tag archive; do
        rm -rf -- "$work/extracted"
        case "$rejected" in
            signature) printf '\n' >> "$work/assets/checksums.txt" ;;
            tag)
                cp "$work/original-checksums" "$work/assets/checksums.txt"
                sign_checksums v1.2.3-rc.2
                ;;
            archive)
                sign_checksums "$tag"
                printf '{"adapters":[]}\n' > "$work/bundle/index.json"
                (cd "$work/bundle" && sha256sum index.json > SHA256SUMS)
                tar -czf "$work/assets/$archive" -C "$work/bundle" .
                ;;
        esac
        if ADAPTER_BUNDLE_URL="https://example.invalid/download/$tag" PATH="$work/bin:$PATH" \
            sh "$work/docker-run" > "$work/docker.log" 2>&1; then
            echo "Accepted adapter bundle with mismatched $rejected" >&2
            exit 1
        fi
        test ! -e "$work/extracted/index.json"
    done
    printf 'ok - refuses a mismatched signature, tag, or archive before extraction\n'
}

case "${1-}" in
    pkgbuild) test_pkgbuild ;;
    docker) test_docker_bundle ;;
    '') test_pkgbuild; test_docker_bundle ;;
    *) exit 2 ;;
esac
