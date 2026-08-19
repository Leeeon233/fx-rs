#!/usr/bin/env bash
set -euo pipefail

fxrs_repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$fxrs_repo_root"

fxrs_mode=${1:---check}
fxrs_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
fxrs_user_agent="fxrs-release-script/$fxrs_version (https://github.com/Leeeon233/fx-rs)"

fxrs_packages=(
    fxrs-core
    fxrs-workspace
    fxrs-provider
    fxrs-auth
    fxrs-config
    fxrs-context
    fxrs-gateway
    fxrs-mcp
    fxrs-process
    fxrs-skills
    fxrs-store
    fxrs-subagent
    fxrs-tools
    fxrs-web
    fxrs-provider-codex
    fxrs-terminal-host
    fxrs-acp-host
    fxrs
)

if [[ -z "$fxrs_version" ]]; then
    echo "publish-crates: could not resolve the workspace version" >&2
    exit 1
fi

fxrs_version_exists() {
    local fxrs_package=$1
    curl --location --silent --fail \
        --user-agent "$fxrs_user_agent" \
        --output /dev/null \
        "https://crates.io/api/v1/crates/$fxrs_package/$fxrs_version"
}

case "$fxrs_mode" in
    --check)
        for fxrs_package in "${fxrs_packages[@]}"; do
            echo "==> inspecting $fxrs_package $fxrs_version"
            fxrs_package_files=$(cargo package \
                --package "$fxrs_package" \
                --locked \
                --allow-dirty \
                --no-verify \
                --list)
            grep --quiet --line-regexp 'Cargo.toml' <<<"$fxrs_package_files"
            grep --quiet --line-regexp 'LICENSE' <<<"$fxrs_package_files"
            grep --quiet --line-regexp 'README.md' <<<"$fxrs_package_files"
        done
        ;;
    --execute)
        if [[ -n "$(git status --porcelain)" ]]; then
            echo "publish-crates: refusing to publish from a dirty worktree" >&2
            exit 1
        fi

        for fxrs_package in "${fxrs_packages[@]}"; do
            if fxrs_version_exists "$fxrs_package"; then
                echo "==> $fxrs_package $fxrs_version is already published"
                continue
            fi

            echo "==> publishing $fxrs_package $fxrs_version"
            cargo publish --package "$fxrs_package" --locked

            fxrs_attempt=0
            until fxrs_version_exists "$fxrs_package"; do
                fxrs_attempt=$((fxrs_attempt + 1))
                if ((fxrs_attempt >= 30)); then
                    echo "publish-crates: $fxrs_package $fxrs_version did not become visible within five minutes" >&2
                    exit 1
                fi
                sleep 10
            done
        done
        ;;
    *)
        echo "usage: scripts/publish-crates.sh [--check|--execute]" >&2
        exit 2
        ;;
esac
