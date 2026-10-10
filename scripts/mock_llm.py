#!/usr/bin/env python3
"""Scripted model server (chat completions, /responses and /v1/messages) for deterministic harness tests.
usage: mock_llm.py PORT  (replies are read from the MOCK_REPLIES env var, a JSON list, served in order)"""
import json, os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
R = json.loads(os.environ["MOCK_REPLIES"]); n = [0]
BY_LEN = os.environ.get("MOCK_BY_LEN") == "1"  # reply index = number of assistant turns so far (stateless across restarts)
class H(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        if os.environ.get("MOCK_LOG"):
            open(os.environ["MOCK_LOG"], "a").write(json.dumps(body) + "\n")
        if BY_LEN:
            idx = sum(1 for m in body.get("messages", body.get("input", [])) if m["role"] == "assistant")
        else:
            idx = n[0]; n[0] += 1
        t = R[min(idx, len(R) - 1)]
        if self.path.endswith("/responses"):  # Responses API shape
            doc = {"output": [{"type": "reasoning", "summary": []}, {"type": "message", "content": [{"type": "output_text", "text": t}]}],
                   "usage": {"input_tokens": 10, "output_tokens": 5, "cost": 0.001}}
        elif self.path.endswith("/v1/messages"):  # Anthropic Messages shape
            doc = {"content": [{"type": "text", "text": t}], "stop_reason": "end_turn", "usage": {"input_tokens": 10, "output_tokens": 5}}
        else:
            doc = {"choices": [{"message": {"content": t}, "finish_reason": "stop"}],
                          "usage": {"prompt_tokens": 10, "completion_tokens": 5, "cost": 0.001}}
        out = json.dumps(doc).encode()
        self.send_response(200); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(out)))
        self.end_headers(); self.wfile.write(out)
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
