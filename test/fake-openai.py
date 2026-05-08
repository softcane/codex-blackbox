#!/usr/bin/env python3
"""Fake OpenAI Responses upstream for Codex Blackbox fixture E2E tests.

This server is intentionally local-only test infrastructure. It accepts
Responses-shaped requests and streams checked-in fixture SSE without contacting
OpenAI or Codex.
"""

from __future__ import annotations

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse


FIXTURES_DIR = Path("/fixtures")
STREAM_FIXTURES = {
    "completed": "openai_responses_text_stream.sse",
    "failed": "openai_responses_failed_stream.sse",
    "incomplete": "openai_responses_incomplete_stream.sse",
}

SPLIT_STREAM_CHUNK_SIZE = 7


def _scenario_from_request(headers, body: dict) -> str:
    explicit = headers.get("x-codex-blackbox-fixture")
    if explicit in STREAM_FIXTURES:
        return explicit

    for key in ("metadata", "client_metadata"):
        value = body.get(key)
        if isinstance(value, dict):
            fixture = value.get("codex_blackbox_fixture")
            if fixture in {"failed", "incomplete"}:
                return fixture
    return "completed"


def _served_model(body: dict) -> str:
    model = body.get("model")
    if isinstance(model, str) and model.strip():
        return model.strip()
    return "gpt-codex-fixture"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self) -> None:
        if urlparse(self.path).path == "/health":
            self._send_plain(200, "ok\n")
            return
        self._send_plain(404, "not found\n")

    def do_POST(self) -> None:
        if urlparse(self.path).path != "/v1/responses":
            self._send_plain(404, "not found\n")
            return

        if self.headers.get("accept-encoding"):
            self._send_json(
                400,
                {
                    "error": {
                        "type": "invalid_request_error",
                        "message": "accept-encoding reached fake OpenAI upstream",
                    }
                },
            )
            return

        length = int(self.headers.get("content-length", "0") or "0")
        raw_body = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw_body)
        except json.JSONDecodeError:
            body = {}

        fixture_name = STREAM_FIXTURES[_scenario_from_request(self.headers, body)]
        stream_path = FIXTURES_DIR / fixture_name
        try:
            stream = stream_path.read_bytes()
        except OSError as err:
            self._send_json(
                500,
                {
                    "error": {
                        "type": "server_error",
                        "message": f"missing fixture {fixture_name}: {err}",
                    }
                },
            )
            return

        served_model = _served_model(body)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("openai-model", served_model)
        self.send_header("x-openai-model", served_model)
        self.send_header("connection", "close")
        self.end_headers()

        if self.headers.get("x-codex-blackbox-split-sse") == "1":
            for offset in range(0, len(stream), SPLIT_STREAM_CHUNK_SIZE):
                self.wfile.write(stream[offset : offset + SPLIT_STREAM_CHUNK_SIZE])
                self.wfile.flush()
                time.sleep(0.005)
        else:
            for event in stream.split(b"\n\n"):
                if not event.strip():
                    continue
                self.wfile.write(event + b"\n\n")
                self.wfile.flush()
                time.sleep(0.02)
        self.close_connection = True

    def _send_plain(self, status: int, body: str) -> None:
        encoded = body.encode()
        self.send_response(status)
        self.send_header("content-type", "text/plain")
        self.send_header("content-length", str(len(encoded)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(encoded)
        self.close_connection = True

    def _send_json(self, status: int, payload: dict) -> None:
        encoded = json.dumps(payload, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(encoded)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(encoded)
        self.close_connection = True

    def log_message(self, fmt: str, *args: object) -> None:
        sys.stderr.write("fake-openai: " + fmt % args + "\n")


if __name__ == "__main__":
    server = ThreadingHTTPServer(("0.0.0.0", 8000), Handler)
    print("fake-openai listening on :8000", flush=True)
    server.serve_forever()
