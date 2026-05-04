#!/usr/bin/env bash
# Static validation for the default ChatGPT/Codex subscription Envoy path.
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

docker compose -f docker-compose.yml config >/tmp/coditor-codex-compose-config.txt

rg -n 'failure_mode_allow:\s*true' envoy/envoy.yaml >/dev/null
rg -n 'request_body_mode:\s*BUFFERED' envoy/envoy.yaml >/dev/null
rg -n 'response_body_mode:\s*STREAMED' envoy/envoy.yaml >/dev/null
rg -n 'prefix:\s*"/backend-api"' envoy/envoy.yaml >/dev/null
rg -n 'host_rewrite_literal:\s*chatgpt\.com' envoy/envoy.yaml >/dev/null
rg -n 'address:\s*chatgpt\.com' envoy/envoy.yaml >/dev/null
rg -n 'sni:\s*chatgpt\.com' envoy/envoy.yaml >/dev/null
if rg -n 'response_code_details' envoy/envoy.yaml test/envoy.openai-responses.e2e.yaml >/dev/null; then
    echo "Codex Envoy access logs must not expose response_code_details for successful streams" >&2
    exit 1
fi

printf "ChatGPT/Codex Envoy/Compose static validation passed\n"
