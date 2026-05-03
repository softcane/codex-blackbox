# Coditor

Coditor observes Codex model-turn traffic through a local Envoy proxy,
`coditor-core`, and the `coditor` CLI.

The product-facing Codex surface is limited to Envoy-observed Responses
request/response facts: request identity, requested and served model, terminal
response status, token usage, context fill, accounting anomalies, and response
summary text when present.

Fake OpenAI Responses fixtures validate local contracts only. Live support
claims require explicit real smoke or dogfood evidence.
