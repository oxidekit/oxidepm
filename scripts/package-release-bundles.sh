#!/usr/bin/env bash
set -euo pipefail

inputs=${1:?usage: package-release-bundles.sh BUNDLES KEY_FILE}
key_file=${2:?missing release signing key file}
tag=${GITHUB_REF_NAME:?missing release tag}
repo=${GITHUB_REPOSITORY:?missing release repository}
version=$(sh "$(dirname -- "$0")/deb-version.sh" "$tag")
release_version=${tag#v}
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
umask 022

sign_bundle() {
    local adapters=$1 target=$2 comment verified_comment key
    comment="oxidepm release bundle tag=$tag target=$target"
    minisign -S -s "$key_file" -m "$adapters/SHA256SUMS" \
        -x "$adapters/SHA256SUMS.minisig" -t "$comment" </dev/null
    for key in \
        'RWQMeVEZKn2/BHzgJLldWbECE2BqKV4a0rmNXEzn5N0lxrSi/dDuFjHC' \
        'RWTuJOozYEG8Y1hYBqUF8CBncU1PUQgqfJ/hr6Zcwgt1cDwfeRrZh+q5'; do
        if verified_comment=$(minisign -V -H -Q -P "$key" \
            -m "$adapters/SHA256SUMS" -x "$adapters/SHA256SUMS.minisig" 2>/dev/null); then
            if [[ "$verified_comment" == "$comment" ]]; then
                return 0
            fi
            printf 'Bundle signature for %s has an unexpected trusted comment\n' "$target" >&2
            return 1
        fi
    done
    printf 'Bundle signature for %s did not verify with a release key\n' "$target" >&2
    return 1
}

package_bundle() {
    local target=$1 arch=$2
    local input="$inputs/oxidepm-$target"
    local pkg="$work/oxidepm_${release_version//-/.}_$arch"

    sign_bundle "$input/adapters" "$target"
    install -dm755 "$pkg/DEBIAN" "$pkg/usr/bin" "$pkg/usr/lib/systemd/user" \
        "$pkg/usr/share/oxidepm/adapters" "$pkg/usr/share/doc/oxidepm"
    install -m0755 "$input/"{oxidepm,oxidepmd,oxidepm-sandbox} "$pkg/usr/bin/"
    install -m0644 "$input/adapters/"* "$pkg/usr/share/oxidepm/adapters/"
    install -m0644 "$input/"{LICENSE-MIT,LICENSE-APACHE} "$pkg/usr/share/doc/oxidepm/"

    cat > "$pkg/DEBIAN/control" <<EOF
Package: oxidepm
Version: $version
Section: utils
Priority: optional
Architecture: $arch
Maintainer: Nayiem Willems <nayiem@mono-labs.org>
Description: Process manager written in Rust with adapter support
 OxidePM supervises processes, handles restart and health policies, and
 supports optional runtime and tooling adapters.
Homepage: https://github.com/$repo
EOF

    cat > "$pkg/DEBIAN/postinst" <<'EOF'
#!/bin/sh
if [ "$1" = configure ]; then
    echo "Import the bundled runtimes with: oxidepm adapter import --bundled --tag runtime --enable --yes"
fi
EOF
    chmod 755 "$pkg/DEBIAN/postinst"

    cat > "$pkg/usr/lib/systemd/user/oxidepmd.service" <<'EOF'
[Unit]
Description=OxidePM Process Manager Daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/oxidepmd
Restart=on-failure
RestartSec=5
TimeoutStopSec=160
KillMode=mixed

[Install]
WantedBy=default.target
EOF

    tar --owner=0 --group=0 --numeric-owner -czf "$input/oxidepm-$target.tar.gz" \
        -C "$pkg/usr/bin" oxidepm oxidepmd oxidepm-sandbox \
        -C "$pkg/usr" share/oxidepm -C "$pkg/usr/share/doc/oxidepm" LICENSE-MIT LICENSE-APACHE
    tar --owner=0 --group=0 --numeric-owner -czf "$input/oxidepm-adapters-$target.tar.gz" \
        -C "$pkg/usr/share/oxidepm/adapters" .
    local deb="$input/${pkg##*/}.deb"
    dpkg-deb --root-owner-group --build "$pkg" "$deb"
    test "$(dpkg-deb -f "$deb" Version)" = "$version"
}

package_bundle x86_64-unknown-linux-gnu amd64
package_bundle aarch64-unknown-linux-gnu arm64
