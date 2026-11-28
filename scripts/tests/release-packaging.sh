#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/oxidepm-packaging-test.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
mkdir "$work/assets" "$work/bin" "$work/bundle"
tag=v1.2.3-rc.1

export TEST_PACKAGING_TAR=$(command -v tar)
cat > "$work/bin/tar" <<'MOCK'
#!/bin/sh
set -eu
if [ "$1" != --no-same-owner ]; then
    echo 'Release extraction must use --no-same-owner' >&2
    exit 1
fi
exec "$TEST_PACKAGING_TAR" "$@"
MOCK
chmod +x "$work/bin/tar"

test_signed_bundles() {
    inputs="$work/inputs"
    mkdir "$inputs"
    export TEST_BUNDLE_MINISIGN=$(command -v minisign)
    minisign -G -W -p "$work/bundle-key.pub" -s "$work/bundle-key.sec" >/dev/null
    export TEST_BUNDLE_KEY=$(tail -n 1 "$work/bundle-key.pub")
    mkdir "$work/signing-bin"
    cat > "$work/signing-bin/minisign" <<'MOCK'
#!/bin/sh
set -eu
if [ "$1" = -V ]; then
    test "$2 $3 $4" = '-H -Q -P'
    # Exercise the second pinned key after the first fails.
    test "$5" = 'RWTuJOozYEG8Y1hYBqUF8CBncU1PUQgqfJ/hr6Zcwgt1cDwfeRrZh+q5'
    shift 5
    set -- -V -H -Q -P "$TEST_BUNDLE_KEY" "$@"
elif [ "${TEST_BUNDLE_WRONG_COMMENT:-}" = 1 ]; then
    test "$1 $8" = '-S -t'
    set -- "$1" "$2" "$3" "$4" "$5" "$6" "$7" "$8" 'wrong trusted comment'
fi
exec "$TEST_BUNDLE_MINISIGN" "$@"
MOCK
    chmod +x "$work/signing-bin/minisign"

    if ! dpkg_deb=$(command -v dpkg-deb); then
        printf '%s\n' 'note - dpkg-deb is unavailable; checking Debian package contents with a tar fixture'
        dpkg_deb="$work/signing-bin/dpkg-deb"
        cat > "$dpkg_deb" <<'MOCK'
#!/bin/sh
set -eu
case "$1" in
    --root-owner-group)
        test "$#" -eq 4 && test "$2" = --build || exit 2
        tar -cf "$4" -C "$3" .
        ;;
    -f)
        test "$#" -eq 3 || exit 2
        case "$3" in Version|Architecture) ;; *) exit 2 ;; esac
        tar -xOf "$2" ./DEBIAN/control | sed -n "s/^$3: //p"
        ;;
    -x)
        test "$#" -eq 3 || exit 2
        tar --no-same-owner -xf "$2" -C "$3"
        ;;
    *) exit 2 ;;
esac
MOCK
        chmod +x "$dpkg_deb"
    fi

    for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
        input="$inputs/oxidepm-$target"
        mkdir -p "$input/adapters"
        for binary in oxidepm oxidepmd oxidepm-sandbox; do
            printf '%s\n' "$binary for $target" > "$input/$binary"
            chmod 0644 "$input/$binary"
        done
        touch "$input/LICENSE-MIT" "$input/LICENSE-APACHE"
        printf '{"target":"%s"}\n' "$target" > "$input/adapters/index.json"
        (cd "$input/adapters" && sha256sum index.json > SHA256SUMS)
    done

    package_bundles() {
        PATH="$work/signing-bin:$PATH" GITHUB_REF_NAME="$tag" GITHUB_REPOSITORY=example/oxidepm \
            bash "$root/scripts/package-release-bundles.sh" "$inputs" "$work/bundle-key.sec"
    }
    if TEST_BUNDLE_WRONG_COMMENT=1 package_bundles > "$work/signing.log" 2>&1; then
        echo 'Packaged a bundle with an unexpected trusted comment' >&2
        exit 1
    fi
    grep -Fq 'has an unexpected trusted comment' "$work/signing.log"
    test ! -e "$inputs/oxidepm-x86_64-unknown-linux-gnu/oxidepm-x86_64-unknown-linux-gnu.tar.gz"
    package_bundles

    for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
        input="$inputs/oxidepm-$target"
        case "$target" in x86_64-*) arch=amd64 ;; aarch64-*) arch=arm64 ;; esac
        full="$work/full-$arch"
        adapters="$work/adapters-$arch"
        deb="$work/deb-$arch"
        package="$input/oxidepm_1.2.3.rc.1_$arch.deb"
        mkdir "$full" "$adapters" "$deb"
        tar --no-same-owner -xzf "$input/oxidepm-$target.tar.gz" -C "$full"
        tar --no-same-owner -xzf "$input/oxidepm-adapters-$target.tar.gz" -C "$adapters"
        "$dpkg_deb" -x "$package" "$deb"
        test "$("$dpkg_deb" -f "$package" Architecture)" = "$arch"
        for dir in "$full/share/oxidepm/adapters" "$adapters" "$deb/usr/share/oxidepm/adapters"; do
            (cd "$dir" && sha256sum --check --strict SHA256SUMS)
            comment=$(minisign -V -H -Q -P "$TEST_BUNDLE_KEY" \
                -m "$dir/SHA256SUMS" -x "$dir/SHA256SUMS.minisig")
            test "$comment" = "oxidepm release bundle tag=$tag target=$target"
            test "$(stat -c %a "$dir/SHA256SUMS.minisig")" = 644
        done
        for dir in "$full" "$deb/usr/bin"; do
            for binary in oxidepm oxidepmd oxidepm-sandbox; do
                test "$(stat -c %a "$dir/$binary")" = 755
            done
        done
    done
    printf 'ok - packages a verified detached adapter signature for each release target\n'
}

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

    src="$work/oxidepm-${tag#v}"
    mkdir -p "$src/target/release" "$work/pkg"
    touch "$src/target/release/oxidepm" "$src/target/release/oxidepmd" \
        "$src/target/release/oxidepm-sandbox" "$src/LICENSE-MIT" "$src/LICENSE-APACHE"
    printf '{}\n' > "$work/bundle/index.json"
    (cd "$work/bundle" && sha256sum index.json > SHA256SUMS)
    printf '%s\n' 'test bundle signature' > "$work/bundle/SHA256SUMS.minisig"
    tar -czf "$work/assets/oxidepm-adapters-$tag-x86_64.tar.gz" -C "$work/bundle" .
    PATH="$work/bin:$PATH" CARCH=x86_64 srcdir="$work/assets" pkgdir="$work/pkg" \
        bash -eu -c 'source "$1"; cd "$2"; package' _ "$work/PKGBUILD" "$work"
    cmp "$work/bundle/index.json" "$work/pkg/usr/share/oxidepm/adapters/index.json"
    cmp "$work/bundle/SHA256SUMS.minisig" "$work/pkg/usr/share/oxidepm/adapters/SHA256SUMS.minisig"
    printf 'ok - extracts the package adapter bundle without archive ownership\n'
}

test_docker_bundle() {
    archive=oxidepm-adapters-x86_64-unknown-linux-gnu.tar.gz
    printf '{}\n' > "$work/bundle/index.json"
    (cd "$work/bundle" && sha256sum index.json > SHA256SUMS)
    printf '%s\n' 'test bundle signature' > "$work/bundle/SHA256SUMS.minisig"
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
    cmp "$work/bundle/SHA256SUMS.minisig" "$work/extracted/SHA256SUMS.minisig"
    printf 'ok - extracts an authenticated adapter bundle without archive ownership\n'

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
    bundle) test_signed_bundles ;;
    pkgbuild) test_pkgbuild ;;
    docker) test_docker_bundle ;;
    '') test_signed_bundles; test_pkgbuild; test_docker_bundle ;;
    *) exit 2 ;;
esac
