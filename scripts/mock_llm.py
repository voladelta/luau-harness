#!/usr/bin/env python3
"""Scripted model server (chat completions and /responses) for deterministic harness tests.
usage: mock_llm.py PORT  (replies are read from the MOCK_REPLIES env var, a JSON list, served in order)
A reply is a string or an object. A string that starts with a ```luau fence becomes one native `luau` tool call with that code
(any text after the fence is the reply text); any other string is a plain-text reply. An object is
{"text": str, "calls": [{"code": str} | {"name": str, "arguments": str}]} for several calls or hand-made (even invalid) arguments.
An object may also carry "prompt_tokens": N (reported as the usage prompt/input tokens, default 10) and, instead of a reply,
"status": 400, "error": "<body>" (answer with that HTTP status and body)."""
import json, os, re, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
R = json.loads(os.environ["MOCK_REPLIES"]); n = [0]
BY_LEN = os.environ.get("MOCK_BY_LEN") == "1"  # reply index = number of assistant turns so far (stateless across restarts)

def parse(t):
    """-> (text, [(name, arguments-json-string)])"""
    if isinstance(t, dict):
        calls = [(c.get("name", "luau"), c["arguments"] if "arguments" in c else json.dumps({"code": c["code"]})) for c in t.get("calls", [])]
        return t.get("text", ""), calls
    m = re.match(r"\s*```luau\n(.*?)\n```\s*(.*)\Z", t, re.S)
    if m:
        return m.group(2), [("luau", json.dumps({"code": m.group(1)}))]
    return t, []

def turns(body):
    """number of assistant turns already in the request"""
    if "input" in body:  # responses: a turn is a message item, or the first function_call of a turn
        return sum(1 for m in body["input"] if m.get("role") == "assistant" or (m.get("type") == "function_call" and m["call_id"].endswith("_0")))
    return sum(1 for m in body.get("messages", []) if m["role"] == "assistant")

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        if os.environ.get("MOCK_LOG"):
            open(os.environ["MOCK_LOG"], "a").write(json.dumps(body) + "\n")
        if BY_LEN:
            idx = turns(body)
        else:
            idx = n[0]; n[0] += 1
        reply = R[min(idx, len(R) - 1)]
        if isinstance(reply, dict) and "status" in reply:
            out = str(reply.get("error", "")).encode()
            self.send_response(reply["status"]); self.send_header("content-length", str(len(out))); self.end_headers(); self.wfile.write(out)
            return
        ptok = reply.get("prompt_tokens", 10) if isinstance(reply, dict) else 10
        text, calls = parse(reply)
        ids = [f"call_{idx}_{k}" for k in range(len(calls))]
        if self.path.endswith("/responses"):  # Responses API shape
            out = [{"type": "reasoning", "summary": []}]
            if text or not calls:
                out.append({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
            out += [{"type": "function_call", "call_id": i, "name": nm, "arguments": a} for i, (nm, a) in zip(ids, calls)]
            doc = {"output": out, "usage": {"input_tokens": ptok, "output_tokens": 5, "cost": 0.001}}
        else:
            msg = {"role": "assistant", "content": text or None}
            if calls:
                msg["tool_calls"] = [{"id": i, "type": "function", "function": {"name": nm, "arguments": a}} for i, (nm, a) in zip(ids, calls)]
            doc = {"choices": [{"message": msg, "finish_reason": "tool_calls" if calls else "stop"}],
                   "usage": {"prompt_tokens": ptok, "completion_tokens": 5, "cost": 0.001}}
        out = json.dumps(doc).encode()
        self.send_response(200); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(out)))
        self.end_headers(); self.wfile.write(out)
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
