#!/usr/bin/env python3
"""A tiny MCP server (stdio, JSON-RPC 2.0) for testing hx's MCP client. Every tools/call is appended to calls.log.
A call to an unlisted tool is a JSON-RPC error (-32602). Files in the working directory switch on test behavior:
demo_extra adds tools (quit: answer, then exit; x-y and x_y, whose safe names collide; a long non-ASCII description),
demo_pages makes tools/list return pages of three tools linked by nextCursor."""
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

EXTRA = [
    {"name": "quit", "description": "Answer, then exit", "inputSchema": {"type": "object"}},
    {"name": "x-y", "description": "Dashed name", "inputSchema": {"type": "object"}},
    {"name": "x_y", "description": "Underscored name", "inputSchema": {"type": "object"}},
    {"name": "long_desc", "description": "\u00e9" * 200, "inputSchema": {"type": "object"}},
]

def tools():
    return TOOLS + (EXTRA if os.path.exists("demo_extra") else [])

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
    if name in ("quit", "x-y", "x_y", "long_desc"):
        return text(name)
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
            listed = tools()
            if os.path.exists("demo_pages"):
                start = int(m.get("params", {}).get("cursor") or 0)
                r = {"tools": listed[start:start + 3]}
                if start + 3 < len(listed):
                    r["nextCursor"] = str(start + 3)
            else:
                r = {"tools": listed}
        elif m["method"] == "tools/call" and m["params"]["name"] not in [t["name"] for t in tools()]:
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32602, "message": "unknown tool: " + m["params"]["name"]}}) + "\n")
            sys.stdout.flush()
            continue
        elif m["method"] == "tools/call":
            r = call(m["params"]["name"], m["params"].get("arguments", {}))
        else:
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "no such method"}}) + "\n")
            sys.stdout.flush()
            continue
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}) + "\n")
        sys.stdout.flush()
        if m["method"] == "tools/call" and m["params"]["name"] == "quit":
            sys.exit(0)

if __name__ == "__main__":
    main()
