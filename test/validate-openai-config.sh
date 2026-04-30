#!/usr/bin/env bash
# Static validation for the Phase 5B manual OpenAI API-key Envoy path.
set -euo pipefail

cd "$(dirname "$0")/.."

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        printf "missing required command: %s\n" "$1" >&2
        exit 1
    }
}

require_cmd docker
require_cmd rg

docker compose -f docker-compose.yml -f docker-compose.openai.yml config >/tmp/coditor-openai-compose-config.txt

rg -n 'failure_mode_allow:\s*true' envoy/envoy.openai.yaml >/dev/null
rg -n 'request_body_mode:\s*BUFFERED' envoy/envoy.openai.yaml >/dev/null
rg -n 'response_body_mode:\s*STREAMED' envoy/envoy.openai.yaml >/dev/null
rg -n 'host_rewrite_literal:\s*api\.openai\.com' envoy/envoy.openai.yaml >/dev/null
rg -n 'address:\s*api\.openai\.com' envoy/envoy.openai.yaml >/dev/null
rg -n 'sni:\s*api\.openai\.com' envoy/envoy.openai.yaml >/dev/null

printf "OpenAI API-key Envoy/Compose static validation passed\n"
