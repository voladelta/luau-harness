#!/usr/bin/env python3
"""Scripted model server (/responses) for deterministic harness tests.
usage: mock_llm.py PORT  (0 picks a free port, written to $PORT_FILE; replies are read from the MOCK_REPLIES env var, a JSON list, served in order)
A reply is a string or an object. A string that starts with a ```luau fence becomes one native `luau` tool call with that code
(any text after the fence is the reply text); any other string is a plain-text reply. An object is
{"text": str, "calls": [{"code": str} | {"name": str, "arguments": str}]} for several calls or hand-made (even invalid) arguments.
An object may also carry "prompt_tokens": N (reported as the usage input tokens, default 10) and, instead of a reply,
"status": 400, "error": "<body>" (answer with that HTTP status and body).
"reasoning": str adds a reasoning summary item that is echoed back on later turns; "finish": "length" marks the reply cut at max_output_tokens."""
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
    return sum(1 for m in body["input"] if m.get("role") == "assistant" or (m.get("type") == "function_call" and m["call_id"].endswith("_0")))

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        assert self.path.endswith("/responses"), self.path
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
        reasoning = reply.get("reasoning") if isinstance(reply, dict) else None
        out = [{"type": "reasoning", "summary": [{"type": "summary_text", "text": reasoning}] if reasoning else []}]
        if text or not calls:
            out.append({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
        out += [{"type": "function_call", "call_id": i, "name": nm, "arguments": a} for i, (nm, a) in zip(ids, calls)]
        doc = {"error": None, "output": out, "usage": {"input_tokens": ptok, "output_tokens": 5}}
        if isinstance(reply, dict) and reply.get("finish") == "length":
            doc["incomplete_details"] = {"reason": "max_output_tokens"}
        out = json.dumps(doc).encode()
        self.send_response(200); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(out)))
        self.end_headers(); self.wfile.write(out)
    def log_message(self, *a): pass
server = HTTPServer(("127.0.0.1", int(sys.argv[1])), H)
# Port 0 binds a free port. With PORT_FILE set, write the port there once listening (atomically: its existence means ready).
if os.environ.get("PORT_FILE"):
    with open(os.environ["PORT_FILE"] + ".tmp", "w") as f: f.write(str(server.server_address[1]))
    os.replace(os.environ["PORT_FILE"] + ".tmp", os.environ["PORT_FILE"])
server.serve_forever()
