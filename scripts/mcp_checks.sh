#!/usr/bin/env bash
# MCP client checks (no API key): binding, schema signatures, approval gate, sandboxed server, env scrubbing, prompt index.
set -u
cd "$(dirname "$0")/.."
R=$PWD; H=$R/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); trap 'rm -rf "$T" "$HOME/hx-mcp-escape.txt"; kill $MOCK $LISTEN 2>/dev/null' EXIT
MOCK=; LISTEN=
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
W=$T/ws; mkdir -p $W/.harness
cfg() { # extra lines for [server.demo]
  printf '[server.demo]\ncommand = "python3"\nargs = ["%s/scripts/mcp_demo_server.py"]\nread_only_tools = ["read_*"]\nenv = ["DEMO_PASS"]\n%s\n' "$R" "${1:-}" > $W/.harness/mcp.toml
  printf '[fs]\nread = ["%s/scripts"]\n%s\n' "$R" "${2:-}" > $W/.harness/policy.toml
}
x() { DEMO_PASS=1 OPENROUTER_API_KEY=sk-secret ENCLAVE_API_KEY=sk-secret2 $H exec --workspace $W "$@" 2>&1 | grep -v "^-- ops="; }
cfg
check "read-only tool (read_*) runs without approval" "$(x 'print(mcp.demo.read_note{name="a"})' | head -1)" "note:a"
check "other tools are external effects: refused without approval" "$(x 'print(pcall(mcp.demo.add, {a = 2, b = 3}))' | head -1 | grep -c approval_required)" "1"
check "the refused call never reached the server" "$(grep -c '^add$' $W/calls.log 2>/dev/null)" "0"
cfg "" 'allow = []
[approval]
allow = ["mcp demo.add"]'
cfg "" '[approval]
allow = ["mcp demo.add"]'
check "user pre-approval in policy.toml lets that tool run" "$(x 'print(mcp.demo.add{a = 2, b = 3})' | head -1)" "5"
check "approval is per tool: delete_note is still refused" "$(x 'print(pcall(mcp.demo.delete_note, {name = "n"}))' | head -1 | grep -c approval_required)" "1"
cfg "" '[approval]
allow = ["mcp demo."]'
check "a server-wide allow covers its tools" "$(x 'print(mcp.demo.delete_note{name = "n"})' | head -1)" "deleted n"
out=$(x 'print(pcall(mcp.demo.boom, {}))' | head -1)
check "a tool error becomes a Luau error with the server's message" "$(echo "$out" | grep -c 'mcp tool error (demo.boom): boom')" "1"
check "tool names are also reachable with a safe alias" "$(x 'print(mcp.demo["read_note"] == mcp.demo.read_note)' | head -1)" "true"

# --- the server itself is sandboxed
check "server cannot write outside the project/temp dir" "$(x 'print(pcall(mcp.demo.write_file, {path = os.getenv and "" or "'$HOME'/hx-mcp-escape.txt", text = "x"}))' >/dev/null; [ -e $HOME/hx-mcp-escape.txt ] && echo ESCAPED || echo blocked)" "blocked"
check "server tool error says the write failed" "$(x 'print(pcall(mcp.demo.write_file, {path = "'$HOME'/hx-mcp-escape.txt", text = "x"}))' | head -1 | grep -c 'cannot write')" "1"
check "server can write inside the project" "$(x 'print(mcp.demo.write_file{path = "ok.txt", text = "x"}) ' | head -1)" "wrote ok.txt"
python3 -c "
import socket,time
s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(5); print(s.getsockname()[1], flush=True); time.sleep(60)" > $T/port & LISTEN=$!; sleep 1; PORT=$(cat $T/port)
check "no network for the server by default" "$(x 'print(pcall(mcp.demo.net_probe, {host = "127.0.0.1", port = '$PORT'}))' | head -1 | grep -c 'no network')" "1"
cfg 'net = true' '[approval]
allow = ["mcp demo."]'
check "net = true in mcp.toml grants it" "$(x 'print(mcp.demo.net_probe{host = "127.0.0.1", port = '$PORT'})' | head -1)" "connected"
cfg "" '[approval]
allow = ["mcp demo."]'
out=$(x 'print(mcp.demo.env_names{})' | head -1)
check "server env: no harness API keys" "$(echo "$out" | grep -c 'API_KEY')" "0"
check "server env: only the variables the user listed pass through" "$(echo "$out" | grep -c 'DEMO_PASS')" "1"
check "the model cannot rewrite mcp.toml" "$(x 'print(pcall(write, ".harness/mcp.toml", "x"))' | head -1 | grep -c protected)" "1"

# --- the model sees generated signatures
MOCK_REPLIES='["done"]' MOCK_BY_LEN=1 MOCK_LOG=$T/req.jsonl python3 scripts/mock_llm.py 8797 & MOCK=$!
sleep 1; echo go > $T/p.txt
OPENROUTER_API_KEY=x $H run --workspace $W --prompt-file $T/p.txt --model m --api openai --base-url http://127.0.0.1:8797 --key-env OPENROUTER_API_KEY >/dev/null 2>&1
sysmsg=$(python3 -c "
import json
l=[json.loads(x) for x in open('$T/req.jsonl')][-1]; m=l['messages'][0]['content']; print(m[0]['text'] if isinstance(m,list) else m)")
check "prompt lists MCP tools with schema-derived signatures" "$(echo "$sysmsg" | grep -c 'mcp.demo.add{a: number, b: number}  -- Add two numbers')" "1"
check "optional/none: a tool with no arguments is mcp.demo.boom{}" "$(echo "$sysmsg" | grep -c 'mcp.demo.boom{}')" "1"
rm $W/.harness/mcp.toml
OPENROUTER_API_KEY=x $H run --workspace $W --prompt-file $T/p.txt --model m --api openai --base-url http://127.0.0.1:8797 --key-env OPENROUTER_API_KEY >/dev/null 2>&1
sysmsg=$(python3 -c "
import json
l=[json.loads(x) for x in open('$T/req.jsonl')][-1]; m=l['messages'][0]['content']; print(m[0]['text'] if isinstance(m,list) else m)")
check "no MCP config: nothing added to the prompt" "$(echo "$sysmsg" | grep -c 'MCP tools')" "0"

# --- Streamable HTTP transport (remote server: url + header auth from env)
HP=8798; HL=$T/hdr.jsonl
hcfg() { # url, extra mcp.toml lines
  printf '[server.web]\nurl = "%s"\nread_only_tools = ["read_*"]\n%s\n' "$1" "${2:-}" > $W/.harness/mcp.toml
  printf '[approval]\nallow = ["mcp web."]\n' > $W/.harness/policy.toml
}
hx() { WEB_KEY=sk-web-secret $H exec --workspace $W "$@" 2>&1 | grep -v "^-- ops="; }
for mode in json sse; do
  (export NEED_HEADER="x-api-key:sk-web-secret" SESSION=1 HDR_LOG=$HL; cd $T; exec python3 $R/scripts/mcp_http_server.py $HP $mode) & SRV=$!; sleep 1; : > $HL
  hcfg "http://127.0.0.1:$HP/mcp" 'headers_env = { "x-api-key" = "WEB_KEY" }'
  check "http/$mode: tool call works with header auth" "$(hx 'print(mcp.web.add{a = 2, b = 3})' | head -1)" "5"
  check "http/$mode: read-only tool runs, tool error is a Luau error" "$(hx 'print(mcp.web.read_note{name="a"}); print(pcall(mcp.web.boom, {}))' | tr '\n' ' ' | grep -c 'note:a.*mcp tool error (web.boom): boom')" "1"
  check "http/$mode: session id is echoed after initialize" "$(python3 -c "
import json
r=[json.loads(l) for l in open('$HL')]; c=[x for x in r if x['_method']=='tools/call']
print(bool(c) and all(x.get('mcp-session-id') for x in c) and all(x.get('mcp-protocol-version')=='2025-06-18' for x in c))")" "True"
  check "http/$mode: initialized notification is sent" "$(grep -c notifications/initialized $HL | awk '{print ($1>0)}')" "1"
  hcfg "http://127.0.0.1:$HP/mcp" 'bearer_env = "WEB_KEY"'
  check "http/$mode: wrong auth scheme surfaces HTTP 401" "$(hx 'print(pcall(mcp.web.add, {a=1,b=1}))' | head -1 | grep -c 'HTTP 401')" "1"
  kill $SRV; wait $SRV 2>/dev/null
done
(export NEED_HEADER="authorization:Bearer sk-web-secret" HDR_LOG=$HL; cd $T; exec python3 $R/scripts/mcp_http_server.py $HP json) & SRV=$!; sleep 1
check "http: bearer_env sends Authorization: Bearer" "$(hx 'print(mcp.web.add{a = 4, b = 5})' | head -1)" "9"
hcfg "http://127.0.0.1:$HP/redir" 'bearer_env = "WEB_KEY"'
check "http: a redirect is not followed" "$(hx 'print(pcall(mcp.web.add, {a=1,b=2}))' | head -1 | grep -c 'redirects are not followed')" "1"
hcfg "http://127.0.0.1:$HP/mcp" 'bearer_env = "NO_SUCH_VAR"'
check "http: unset env var fails clearly before any request" "$(hx 'print(pcall(mcp.web.add, {a=1,b=2}))' | head -1 | grep -c 'NO_SUCH_VAR.*not set')" "1"
hcfg "http://127.0.0.1:$HP/mcp" 'bearer_env = "WEB_KEY"'
hx 'print(mcp.web.add{a = 1, b = 1})' >/dev/null; hx 'print(mcp.web.add{a = 1, b = 1})' >/dev/null
check "http: the key never reaches the event log" "$(grep -rl sk-web-secret $W/.harness 2>/dev/null | grep -v 'mcp.toml\|policy.toml' | wc -l | tr -d ' ')" "0"
kill $SRV; wait $SRV 2>/dev/null
hcfg "https://127.0.0.1:1/mcp" ''
check "http: unreachable server is a Luau error, VM survives" "$(hx 'print(pcall(function() return mcp.web.add{a=1,b=2} end)); print("alive")' | tail -1)" "alive"
hcfg "http://example.com/mcp" ''
check "http: plain http to a remote host is refused" "$(hx 'print(1)' | grep -c 'must be https')" "1"
hcfg "https://example.com/mcp" 'command = "python3"'
check "http: command and url together are refused" "$(hx 'print(1)' | grep -c 'mutually exclusive')" "1"
hcfg "https://example.com/mcp" 'net = true'
check "http: sandbox-only keys are refused on a url server" "$(hx 'print(1)' | grep -c 'do not apply')" "1"

# --- HTTP edge cases: split SSE stream, server request id collision, echoed secret, session expiry
(export SESSION=1 EXPIRE=$T/expire HDR_LOG=$HL; cd $T; exec python3 $R/scripts/mcp_http_server.py $HP sse-split) & SRV=$!; sleep 1; : > $HL
hcfg "http://127.0.0.1:$HP/mcp" 'bearer_env = "WEB_KEY"'
check "http/sse-split: 3-byte chunks, server request with our id, no final blank line" "$(hx 'print(mcp.web.add{a = 6, b = 7})' | head -1)" "13"
touch $T/expire
check "http: session expiry (404) re-initializes and the call succeeds on retry" "$(hx 'print(pcall(function() return mcp.web.add{a=1,b=1} end)); print(pcall(function() return mcp.web.add{a=1,b=2} end))' | tr '\n' ' ' | grep -c 'session expired.*true.*3')" "1"
kill $SRV; wait $SRV 2>/dev/null
(export NEED_HEADER="authorization:Bearer other-key" ECHO_KEY=1; cd $T; exec python3 $R/scripts/mcp_http_server.py $HP json) & SRV=$!; sleep 1
out=$(hx 'print(pcall(function() return mcp.web.add{a=1,b=1} end))' | head -1)
check "http: a 401 body that echoes the secret is scrubbed" "$(echo "$out" | grep -c 'sk-web-secret')" "0"
check "http: ...and the status is still reported" "$(echo "$out" | grep -c 'HTTP 401.*\*\*\*')" "1"
kill $SRV; wait $SRV 2>/dev/null
hcfg "https://example.com/mcp" 'headers_env = { "accept" = "WEB_KEY" }'
check "http: headers the client sets itself cannot be overridden" "$(hx 'print(1)' | grep -c 'cannot be overridden')" "1"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
