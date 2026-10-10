#!/usr/bin/env python3
"""The demo MCP tools over Streamable HTTP, for testing hx's HTTP transport.
usage: mcp_http_server.py PORT [json|sse] ; env: NEED_HEADER=name:value (401 without it), HDR_LOG=file (one JSON line of
request headers per POST), SESSION=1 (issue Mcp-Session-Id, 404 on a wrong one), ECHO_KEY=1 (401 body echoes the
rejected header value), EXPIRE=file (if it exists: delete it, rotate the session, 404 once). Mode sse-split sends the
stream in 3-byte chunks, ends without the final blank line, and precedes the response with a server request using id 1. POST /redir answers 302 to /elsewhere."""
import json, os, sys, uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mcp_demo_server import TOOLS, call

MODE = sys.argv[2] if len(sys.argv) > 2 else "json"
NEED = os.environ.get("NEED_HEADER"); SID = str(uuid.uuid4()) if os.environ.get("SESSION") else None
INITS = [0]

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, code, body=b"", ctype="application/json", extra=()):
        self.send_response(code); self.send_header("Content-Type", ctype); self.send_header("Content-Length", str(len(body)))
        for k, v in extra: self.send_header(k, v)
        self.end_headers(); self.wfile.write(body)
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0)); m = json.loads(self.rfile.read(n) or b"{}")
        if os.environ.get("HDR_LOG"):
            with open(os.environ["HDR_LOG"], "a") as f: f.write(json.dumps({k.lower(): v for k, v in self.headers.items()} | {"_path": self.path, "_method": m.get("method")}) + "\n")
        if self.path == "/redir": return self.reply(302, extra=[("Location", "http://127.0.0.1:1/elsewhere")])
        if NEED:
            k, v = NEED.split(":", 1)
            if self.headers.get(k) != v:
                got = self.headers.get(k) or ""
                return self.reply(401, json.dumps({"error": "bad key " + got if os.environ.get("ECHO_KEY") else "unauthorized"}).encode())
        global SID
        exp = os.environ.get("EXPIRE")
        if SID and exp and os.path.exists(exp) and m.get("method") == "tools/call":
            os.remove(exp); SID = str(uuid.uuid4()); return self.reply(404)
        if SID and m.get("method") != "initialize" and self.headers.get("Mcp-Session-Id") != SID: return self.reply(404)
        if "id" not in m: return self.reply(202)
        meth = m["method"]; extra = [("Mcp-Session-Id", SID)] if SID and meth == "initialize" else []
        if meth == "initialize": INITS[0] += 1
        if meth == "initialize": r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "demo", "version": "1"}}
        elif meth == "tools/list": r = {"tools": TOOLS}
        elif meth == "tools/call": r = call(m["params"]["name"], m["params"].get("arguments", {}))
        else: return self.reply(200, json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "no such method"}}).encode(), extra=extra)
        out = json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r})
        if MODE == "sse-split":
            req = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "ping"})
            body = f"data: {req}\n\ndata: {out}".encode()  # no final blank line
            self.send_response(200); self.send_header("Content-Type", "text/event-stream"); self.send_header("Connection", "close")
            for k, v in extra: self.send_header(k, v)
            self.end_headers()
            for i in range(0, len(body), 3): self.wfile.write(body[i:i+3]); self.wfile.flush()
            self.close_connection = True
        elif MODE == "sse":
            note = json.dumps({"jsonrpc": "2.0", "method": "notifications/message", "params": {}})
            self.reply(200, f"event: message\r\ndata: {note}\r\n\r\nevent: message\r\ndata: {out}\r\n\r\n".encode(), "text/event-stream", extra)
        else: self.reply(200, out.encode(), extra=extra)

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
