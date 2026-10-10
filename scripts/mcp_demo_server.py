#!/usr/bin/env python3
"""A tiny MCP server (stdio, JSON-RPC 2.0) for testing hx's MCP client. Every tools/call is appended to calls.log."""
import json, os, socket, sys

def schema(**props):
    return {"type": "object", "properties": {k: {"type": v} for k, v in props.items()}, "required": list(props)}

TOOLS = [
    {"name": "add", "description": "Add two numbers", "inputSchema": schema(a="number", b="number")},
    {"name": "read_note", "description": "Read a note (read-only)", "inputSchema": schema(name="string")},
    {"name": "delete_note", "description": "Delete a note (destructive)", "inputSchema": schema(name="string")},
    {"name": "write_file", "description": "Write text to a path", "inputSchema": schema(path="string", text="string")},
    {"name": "net_probe", "description": "Try an outbound TCP connection", "inputSchema": schema(host="string", port="number")},
    {"name": "env_names", "description": "List environment variable names", "inputSchema": {"type": "object"}},
    {"name": "boom", "description": "Always fails", "inputSchema": {"type": "object"}},
]

def text(t, err=False):
    return {"content": [{"type": "text", "text": t}], "isError": err}

def call(name, a):
    with open("calls.log", "a") as f:
        f.write(name + "\n")
    if name == "add":
        return text(str(a["a"] + a["b"]).rstrip("0").rstrip(".") if isinstance(a["a"] + a["b"], float) else str(a["a"] + a["b"]))
    if name == "read_note":
        return text("note:" + a["name"])
    if name == "delete_note":
        return text("deleted " + a["name"])
    if name == "write_file":
        try:
            with open(a["path"], "w") as f:
                f.write(a["text"])
            return text("wrote " + a["path"])
        except OSError as e:
            return text("cannot write: " + str(e), True)
    if name == "net_probe":
        try:
            socket.create_connection((a["host"], int(a["port"])), timeout=2).close()
            return text("connected")
        except OSError as e:
            return text("no network: " + str(e), True)
    if name == "env_names":
        return text(",".join(sorted(os.environ)))
    if name == "boom":
        return text("boom", True)
    return text("unknown tool", True)

def main():
    for line in sys.stdin:
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if "id" not in m:
            continue  # notification
        if m["method"] == "initialize":
            r = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "demo", "version": "1"}}
        elif m["method"] == "tools/list":
            r = {"tools": TOOLS}
        elif m["method"] == "tools/call":
            r = call(m["params"]["name"], m["params"].get("arguments", {}))
        else:
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "no such method"}}) + "\n")
            sys.stdout.flush()
            continue
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}) + "\n")
        sys.stdout.flush()

if __name__ == "__main__":
    main()
