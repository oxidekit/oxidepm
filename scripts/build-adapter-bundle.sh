#!/usr/bin/env bash
set -euo pipefail

# Run from the adapters checkout, next to the core checkout.
adapters=(
    check
    deploy
    exporter-prometheus
    git-start
    init
    notifier
    runtime-go
    runtime-node
    runtime-python
    runtime-rust
    ui-tui
    ui-web
    watch
)

if [[ ${1-} == --list && $# -eq 1 ]]; then
    printf '%s\n' "${adapters[@]}"
    exit 0
fi

target=${1:?usage: build-adapter-bundle.sh TARGET BUNDLE_DIR URL_BASE SEQUENCE}
bundle=${2:?missing bundle directory}
url_base=${3:?missing catalog URL base}
sequence=${4:?missing catalog sequence}
bin="${CARGO_TARGET_DIR:-target}/$target/release"

packages=(-p oxidepm-adapter-bundle)
inputs=()
for name in "${adapters[@]}"; do
    packages+=(-p "oxidepm-adapter-$name")
    inputs+=("$name:$bin/oxidepm-$name")
done

cargo build --locked --release --target "$target" "${packages[@]}"
"$bin/oxidepm-adapter-bundle" --out "$bundle" --target "$target" \
    --url-base "$url_base" --sequence "$sequence" "${inputs[@]}"
(cd "$bundle" && shasum -a 256 -c SHA256SUMS)
