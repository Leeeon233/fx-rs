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
    fxrs-mcp
    fxrs-process
    fxrs-store
    fxrs-subagent
    fxrs-tools
    fxrs-terminal-host
    fxrs-runtime
    fxrs-tui
    fxrs
)

if [[ -z "$fxrs_version" ]]; then
    echo "publish-crates: could not resolve the workspace version" >&2
    exit 1
fi

fxrs_index_path() {
    local fxrs_package=$1
    case ${#fxrs_package} in
        1) printf '1/%s' "$fxrs_package" ;;
        2) printf '2/%s' "$fxrs_package" ;;
        3) printf '3/%s/%s' "${fxrs_package:0:1}" "$fxrs_package" ;;
        *) printf '%s/%s/%s' "${fxrs_package:0:2}" "${fxrs_package:2:2}" "$fxrs_package" ;;
    esac
}

# Returns 0 when the version exists, 1 when it is absent, and 2 when the
# registry index could not be queried. Keeping lookup failures distinct avoids
# attempting an irreversible duplicate publish during a crates.io outage.
fxrs_version_exists() {
    local fxrs_package=$1
    local fxrs_index_response
    local fxrs_http_code
    local fxrs_index_body
    local fxrs_index_url="https://index.crates.io/$(fxrs_index_path "$fxrs_package")"

    if ! fxrs_index_response=$(curl --location --silent --show-error \
        --user-agent "$fxrs_user_agent" \
        --header 'Cache-Control: no-cache' \
        --write-out $'\n%{http_code}' \
        "$fxrs_index_url"); then
        return 2
    fi
    fxrs_http_code=${fxrs_index_response##*$'\n'}
    fxrs_index_body=${fxrs_index_response%$'\n'*}
    case "$fxrs_http_code" in
        200)
            grep --fixed-strings --quiet \
                "\"name\":\"$fxrs_package\",\"vers\":\"$fxrs_version\"" \
                <<<"$fxrs_index_body"
            ;;
        404) return 1 ;;
        *)
            echo "publish-crates: sparse index returned HTTP $fxrs_http_code for $fxrs_package" >&2
            return 2
            ;;
    esac
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
    --status)
        fxrs_missing=0
        for fxrs_package in "${fxrs_packages[@]}"; do
            if fxrs_version_exists "$fxrs_package"; then
                echo "published  $fxrs_package $fxrs_version"
            else
                fxrs_lookup_status=$?
                if ((fxrs_lookup_status == 2)); then
                    exit 1
                fi
                echo "missing    $fxrs_package $fxrs_version"
                fxrs_missing=1
            fi
        done
        exit "$fxrs_missing"
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
            else
                fxrs_lookup_status=$?
                if ((fxrs_lookup_status == 2)); then
                    exit 1
                fi
            fi

            echo "==> publishing $fxrs_package $fxrs_version"
            cargo publish --package "$fxrs_package" --locked

            fxrs_attempt=0
            while ! fxrs_version_exists "$fxrs_package"; do
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
        echo "usage: scripts/publish-crates.sh [--check|--status|--execute]" >&2
        exit 2
        ;;
esac
