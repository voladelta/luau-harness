#!/usr/bin/env bash
# P2 acceptance checks (no API key needed). Run after `cargo build --release`.
set -u
cd "$(dirname "$0")/.."
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
H=$PWD/target/release/hx
T=$(mktemp -d); trap 'rm -rf "$T"; kill $MOCK 2>/dev/null' EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }

# 1. std package tests (printer, hooks, mcp)
out=$($H test stdlib/std --workspace "$T" | tail -1); check "std tests" "$out" "17 passed, 0 failed"

# 2. override the printer in one project, no rebuild; removing the override restores the default
W=$T/ws; mkdir -p $W/.harness/override/std
base=$($H exec --workspace $W 'print({1,2})' | head -1)
cat > $W/.harness/override/std/printer.luau <<'LUA'
local base = require("base:std/printer")
local M = {}
for k, v in base do M[k] = v end
function M.top(v) return "[custom] " .. base.top(v) end
return M
LUA
over=$($H exec --workspace $W 'print({1,2})' | head -1)
check "printer override active" "$over" "[custom] $base"
rm $W/.harness/override/std/printer.luau
check "override removed" "$($H exec --workspace $W 'print({1,2})' | head -1)" "$base"

# 2b. a broken override falls back to the built-in module instead of breaking the harness
mkdir -p $W/.harness/override/std
echo 'return 1' > $W/.harness/override/std/printer.luau
check "override returning a non-table falls back" "$($H exec --workspace $W 'print({1,2})' 2>/dev/null | head -1)" "$base"
echo 'this is not luau (((' > $W/.harness/override/std/printer.luau
check "override with a syntax error falls back" "$($H exec --workspace $W 'print({1,2})' 2>/dev/null | head -1)" "$base"
check "fallback is reported on stderr" "$($H exec --workspace $W 'print(1)' 2>&1 >/dev/null | grep -c 'is broken')" "1"
rm $W/.harness/override/std/printer.luau

# 3. parallel is concurrent: 3 x sleep 2 finishes in < 4s
s=$(date +%s); $H exec --workspace $W 'parallel({function() return sh("sleep 2").code end, function() return sh("sleep 2").code end, function() return sh("sleep 2").code end})' >/dev/null; e=$(date +%s)
check "parallel concurrent (<4s)" "$([ $((e-s)) -lt 4 ] && echo yes || echo no)" "yes"

# 4. vars() lists session variables
check "vars" "$($H exec --workspace $W $'x = 41\nvars()' | head -1)" "=> <table: 1 keys> {x=41}"

# 5. package lock + grants gate
P=$W/.harness/pkgs/mypkg; mkdir -p $P/tests
printf '[package]\nname = "mypkg"\nversion = "0.1.0"\n[capabilities]\nnet = ["example.com"]\n' > $P/hx.toml
echo 'return { hello = function() return "hi" end }' > $P/init.luau
echo 'test.case("loads", function() test.eq(require("mypkg").hello(), "hi") end)' > $P/tests/t.luau
check "unlocked package refused" "$($H test $P --workspace $W 2>&1 | grep -c 'is not locked')" "1"
$H approve mypkg --workspace $W >/dev/null
check "approved package loads" "$($H test $P --workspace $W | tail -1)" "1 passed, 0 failed"
echo 'return { hello = function() return "evil" end }' > $P/init.luau
check "tampered package refused" "$($H test $P --workspace $W 2>&1 | grep -c 'changed since it was approved')" "1"
# a failed lock check must fail on EVERY require, not just the first
check "tampered package refused on second require" "$($H exec --workspace $W $'local t = deftool("twice", [[return { go = function() local a = pcall(require, "mypkg"); local b = pcall(require, "mypkg"); return tostring(a) .. tostring(b) end }]])\nt.go()' | head -1)" "=> falsefalse"

# 6. hooks can deny, subagents run, usage aggregates (scripted model)
W2=$T/ws2; mkdir -p $W2/.harness; echo hi > $W2/readme.txt
cat > $W2/.harness/init.luau <<'LUA'
require("std/hooks").on("before_run", function(p)
	if p.code:find("rm -rf", 1, true) then return { deny = "no rm -rf here" } end
end)
LUA
MOCK_REPLIES='["```luau\nsh(\"rm -rf /tmp/zzz-hx\")\n```","```luau\nlocal r = agent(\"sub\")\nprint(\"sub said: \" .. r.answer)\n```","```luau\nprint(1)\n```","SUB-ANSWER","All done."]' python3 scripts/mock_llm.py 8768 & MOCK=$!
sleep 1; echo go > $T/p.txt
OPENROUTER_API_KEY=x $H run --workspace $W2 --prompt-file $T/p.txt --model m --api openai --base-url http://127.0.0.1:8768 --key-env OPENROUTER_API_KEY --trace $T/trace.txt > $T/rep.json
check "hook denied rm -rf" "$(grep -c 'no rm -rf here' $T/trace.txt)" "1"
check "subagent answer reached parent" "$(grep -c "sub said: SUB-ANSWER" $T/trace.txt)" "1"
check "usage aggregated across subagent (5 llm calls)" "$(python3 -c "import json;print(json.load(open('$T/rep.json'))['llm_calls'])")" "5"

# 7. hot reload keeps session state
W3=$T/ws3; mkdir -p $W3/pk/tests $W3/.harness/override/std
cat > $W3/pk/tests/reload_test.luau <<'LUA'
test.case("hot reload swaps the printer", function()
	test.eq(__format(5), "5")
	write(".harness/override/std/printer.luau", 'local b = require("base:std/printer")\nlocal M = {}\nfor k, v in b do M[k] = v end\nfunction M.top(v) return "X" .. b.top(v) end\nreturn M')
	test.eq(__format(5), "5")
	reload("std/printer")
	test.eq(__format(5), "X5")
end)
LUA
check "hot reload" "$($H test $W3/pk --workspace $W3 --no-sandbox 2>/dev/null | tail -1)" "1 passed, 0 failed"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
