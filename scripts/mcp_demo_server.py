#!/usr/bin/env python3
"""A tiny MCP server (stdio, JSON-RPC 2.0) for testing hx's MCP client. Every tools/call is appended to calls.log.
Over stdio, tools/list answers in pages of three (nextCursor); `exit` answers, closes its input, creates `exited` and
then exits, so a check can wait for `exited` and know the next write finds no reader; `ask_ping` sends the
client a ping request and answers only once the client has replied to it. `rpc_error` answers with a JSON-RPC error.
`read_spawn` starts a long-lived child (as a wrapper like npx would) and writes its pid to spawned.pid."""
import json, os, socket, subprocess, sys

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
    {"name": "rpc_error", "description": "Answers with a JSON-RPC error", "inputSchema": {"type": "object"}},
    {"name": "exit", "description": "Answers, then the server exits", "inputSchema": {"type": "object"}},
    {"name": "ask_ping", "description": "Pings the client before answering", "inputSchema": {"type": "object"}},
    {"name": "read_spawn", "description": "Starts a long-lived child process", "inputSchema": {"type": "object"}},
]
PAGE = 3


class RpcError(Exception):
    """A tools/call answered with a JSON-RPC error instead of a result."""

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
    if name == "exit":
        return text("bye")
    if name == "read_spawn":
        child = subprocess.Popen(["sleep", "300"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        with open("spawned.pid", "w") as f:
            f.write(str(child.pid))
        return text("spawned")
    if name == "rpc_error":
        raise RpcError("Invalid params: missing key")
    return text("unknown tool", True)

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def await_reply(lines, rid):
    """Read until the client's response to our request `rid`; True if it answered with a result."""
    for line in lines:
        m = json.loads(line)
        if m.get("id") == rid and "method" not in m:
            return "result" in m
    return False


def main():
    lines = iter(sys.stdin)
    for line in lines:
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if "id" not in m:
            continue  # notification
        if m["method"] == "initialize":
            r = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "demo", "version": "1"}}
        elif m["method"] == "tools/list":
            start = int(m.get("params", {}).get("cursor") or 0)
            r = {"tools": TOOLS[start:start + PAGE]}
            if start + PAGE < len(TOOLS):
                r["nextCursor"] = str(start + PAGE)
        elif m["method"] == "tools/call" and m["params"]["name"] == "ask_ping":
            send({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})
            r = text("pong received" if await_reply(lines, "srv-1") else "no pong", False)
        elif m["method"] == "tools/call":
            try:
                r = call(m["params"]["name"], m["params"].get("arguments", {}))
            except RpcError as e:
                send({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32602, "message": str(e)}})
                continue
        else:
            send({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "no such method"}})
            continue
        send({"jsonrpc": "2.0", "id": m["id"], "result": r})
        if m["method"] == "tools/call" and m["params"]["name"] == "exit":
            os.close(0)
            open("exited", "w").close()
            sys.exit(0)


if __name__ == "__main__":
    main()
