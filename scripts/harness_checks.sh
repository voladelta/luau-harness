#!/usr/bin/env bash
# Harness checks: std tests, module overrides, sealed batches, package locks, hooks and subagents (no API key needed). Run after `cargo build --release`.
set -u
cd "$(dirname "$0")/.."
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
H=$PWD/target/release/hx
T=$(mktemp -d); trap 'rm -rf "$T"; kill $MOCK 2>/dev/null' EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }

# 1. std package tests (printer, hooks, mcp)
out=$($H test stdlib/std --workspace "$T" | tail -1); check "std tests" "$out" "31 passed, 0 failed"

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

# 3. Only sealed read batches are available for concurrency.
check "parallel API absent" "$($H exec --workspace $W 'print(parallel == nil)' 2>&1 | head -1)" "true"
echo batch-data > $W/batch.txt
check "sealed read batch" "$($H exec --workspace $W 'print(batch({{op="read",args={"batch.txt"}}})[1].value)' | head -1)" "batch-data"

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
mkdir -p $W/probe/tests
cat > $W/probe/tests/t.luau <<'LUA'
test.case("failed require never enters a cache",function()
 local a=pcall(require,"mypkg");local b=pcall(require,"mypkg")
 test.ok(not a and not b)
end)
LUA
check "tampered package refused on second require" "$($H test $W/probe --workspace $W | tail -1)" "1 passed, 0 failed"

# 6. hooks can deny, subagents run, usage aggregates (scripted model)
W2=$T/ws2; mkdir -p $W2/.harness; echo hi > $W2/readme.txt
cat > $W2/.harness/init.luau <<'LUA'
require("std/recheck").enabled = false -- this scenario tests hooks and shared usage
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

# 6b. a before_finish hook can send the model back to work once
W4=$T/ws4; mkdir -p $W4/.harness
cat > $W4/.harness/init.luau <<'LUA'
require("std/recheck").enabled = false -- isolate the custom before_finish hook
local hooks = require("std/hooks")
hooks.on("before_finish", function(p)
	if hooks.state.asked then return { action = "accept" } end
	hooks.state.asked = true
	return { action = "continue", message = "Check once more." }
end)
LUA
kill $MOCK 2>/dev/null; wait $MOCK 2>/dev/null
MOCK_REPLIES='["```luau\nprint(1)\n```","First answer.","Checked: final answer."]' python3 scripts/mock_llm.py 8768 & MOCK=$!
sleep 1
OPENROUTER_API_KEY=x $H run --workspace $W4 --prompt-file $T/p.txt --model m --api openai --base-url http://127.0.0.1:8768 --key-env OPENROUTER_API_KEY --trace $T/trace4.txt > $T/rep4.json
check "before_finish continues once" "$(python3 -c "import json;r=json.load(open('$T/rep4.json'));print(r['llm_calls'], r['final_answer'])")" "3 Checked: final answer."

# 6d. recheck is on by default and sends the model back exactly once
W6=$T/ws6; mkdir -p $W6
kill $MOCK 2>/dev/null; wait $MOCK 2>/dev/null
MOCK_REPLIES='["```luau\nprint(1)\n```","First answer.","Checked: final answer."]' python3 scripts/mock_llm.py 8768 & MOCK=$!
sleep 1
OPENROUTER_API_KEY=x $H run --workspace $W6 --prompt-file $T/p.txt --model m --api openai --base-url http://127.0.0.1:8768 --key-env OPENROUTER_API_KEY --trace $T/trace6.txt > $T/rep6.json
check "default recheck continues once" "$(python3 -c "import json;r=json.load(open('$T/rep6.json'));print(r['llm_calls'], r['final_answer'])") $(grep -c 'state the root cause you fixed' $T/trace6.txt)" "3 Checked: final answer. 1"

W7=$T/ws7; mkdir -p $W7/.harness
echo 'require("std/recheck").enabled = false' > $W7/.harness/init.luau
kill $MOCK 2>/dev/null; wait $MOCK 2>/dev/null
MOCK_REPLIES='["```luau\nprint(1)\n```","First answer.","Checked: final answer."]' python3 scripts/mock_llm.py 8768 & MOCK=$!
sleep 1
OPENROUTER_API_KEY=x $H run --workspace $W7 --prompt-file $T/p.txt --model m --api openai --base-url http://127.0.0.1:8768 --key-env OPENROUTER_API_KEY --trace $T/trace7.txt > $T/rep7.json
check "recheck opt-out accepts the first final answer" "$(python3 -c "import json;r=json.load(open('$T/rep7.json'));print(r['llm_calls'], r['final_answer'])") $(grep -c 'state the root cause you fixed' $T/trace7.txt)" "2 First answer. 0"

# 6c. Lost or malformed generations are not automatically retried (and retain reservations).
HX_CHECK_FILTER="lost model response" python3 scripts/redesign_checks.py >/dev/null
check "lost generations are not retried" "$?" "0"

# 7. a live run keeps its frozen source and session state
W3=$T/ws3; mkdir -p $W3/pk/tests $W3/.harness/override/std
cat > $W3/pk/tests/reload_test.luau <<'LUA'
test.case("a run keeps its frozen printer", function()
	test.eq(__format(5), "5")
	-- the override is user-owned (protected even under --yolo): signal the script, which writes it like a user would
	write("ready", "1")
	for _ = 1, 100 do
		if pcall(read, ".harness/override/std/printer.luau") then break end
		sh("sleep 0.05")
	end
	test.eq(__format(5), "5")
	test.eq(reload, nil)
	test.eq(__format(5), "5")
end)
LUA
( for _ in $(seq 100); do [ -f $W3/ready ] && break; sleep 0.05; done
  printf 'local b = require("base:std/printer")\nlocal M = {}\nfor k, v in b do M[k] = v end\nfunction M.top(v) return "X" .. b.top(v) end\nreturn M' > $W3/.harness/override/std/printer.luau ) &
check "frozen printer refuses hot reload" "$($H test $W3/pk --workspace $W3 2>/dev/null | tail -1)" "1 passed, 0 failed"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
