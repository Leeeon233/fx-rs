#!/usr/bin/env bash
set -euo pipefail

fxrs_repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$fxrs_repo_root"

fxrs_target=${1:-$(rustc -vV | sed -n 's/^host: //p')}
fxrs_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)

if [[ -z "$fxrs_target" || -z "$fxrs_version" ]]; then
    echo "package-release: could not resolve target or version" >&2
    exit 1
fi

case "$fxrs_target" in
    x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu | x86_64-apple-darwin | aarch64-apple-darwin) ;;
    *)
        echo "package-release: unsupported release target: $fxrs_target" >&2
        exit 1
        ;;
esac

fxrs_name="fxrs-v${fxrs_version}-${fxrs_target}"
fxrs_dist="$fxrs_repo_root/dist"
fxrs_archive="$fxrs_dist/$fxrs_name.tar.gz"
fxrs_checksum="$fxrs_archive.sha256"

if [[ -e "$fxrs_archive" || -e "$fxrs_checksum" ]]; then
    echo "package-release: refusing to overwrite an existing $fxrs_name artifact" >&2
    exit 1
fi

cargo build --release --locked --workspace --target "$fxrs_target"

fxrs_stage=$(mktemp -d "${TMPDIR:-/tmp}/fxrs-release.XXXXXX")
trap 'rm -rf -- "$fxrs_stage"' EXIT
fxrs_package="$fxrs_stage/$fxrs_name"
mkdir -p "$fxrs_package" "$fxrs_dist"

for fxrs_binary in fxrs fx-tui fx-acp fx-terminal-host; do
    fxrs_source="$fxrs_repo_root/target/$fxrs_target/release/$fxrs_binary"
    if [[ ! -x "$fxrs_source" ]]; then
        echo "package-release: missing executable: $fxrs_source" >&2
        exit 1
    fi
    install -m 755 "$fxrs_source" "$fxrs_package/$fxrs_binary"
done

install -m 644 README.md LICENSE CHANGELOG.md "$fxrs_package/"
"$fxrs_package/fxrs" --version >/dev/null
"$fxrs_package/fxrs" tui --help >/dev/null
"$fxrs_package/fxrs" acp --help >/dev/null

COPYFILE_DISABLE=1 tar -C "$fxrs_stage" -czf "$fxrs_archive" "$fxrs_name"

(
    cd "$fxrs_dist"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$(basename "$fxrs_archive")" >"$(basename "$fxrs_checksum")"
    else
        shasum -a 256 "$(basename "$fxrs_archive")" >"$(basename "$fxrs_checksum")"
    fi
)

printf '%s\n%s\n' "$fxrs_archive" "$fxrs_checksum"
