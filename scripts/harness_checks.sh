#!/usr/bin/env bash
# Harness checks: std tests, module overrides, sealed batches, package locks, hooks and subagents (no API key needed). Run after `cargo build --release`.
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
cleanup() { kill $MOCK 2>/dev/null; }

# 1. std package tests (printer, hooks, mcp)
out=$($H test stdlib/std --workspace "$T"); status=$?
if [ "$status" -eq 0 ] && [[ "$out" =~ ^[1-9][0-9]*\ passed,\ 0\ failed$ ]]; then
  check "std tests" "passed" "passed"
else
  check "std tests" "$out" "a nonempty passing suite"
fi

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
check "batch operations count as ops and carry search truncation" "$($H exec --workspace $W 'local r=batch({{op="read",args={"batch.txt"}},{op="search",args={"batch",{path="batch.txt",max=1}}}}); print(r[2].ok, #r[2].value, r[2].truncated)' | head -2 | tr '\n' ' ')" "true	1	false -- ops=2 error=false "

# 3b. An effect called where Luau cannot yield fails before it exists: no command, no held mutation permit, no open intent.
W4=$T/ws4; mkdir -p $W4
out=$(timeout 30 $H exec --workspace $W4 'print(pcall(string.gsub,"a","a",function() return sh("touch ran").stdout end))' 'print(pcall(sh,"echo again"))' 2>&1)
check "effect in a gsub callback is refused" "$(echo "$out" | grep -c 'attempt to yield across metamethod/C-call boundary')" "1"
check "effect in a gsub callback does not run" "$([ -e $W4/ran ] && echo ran || echo absent)" "absent"
check "a later mutation still runs" "$(echo "$out" | grep -cF 'stdout="again\n"')" "1"
check "no intent is left without an outcome" "$(python3 -c '
import json, sys, glob
seen = [json.loads(l) for f in glob.glob(sys.argv[1] + "/*.jsonl") for l in open(f)]
print(len({r["seq"] for r in seen if r["kind"] == "intent"} - {r.get("of") for r in seen}))' "$(logdir $W4)")" "0"

# 3c. search returns plain data: capped rows cross the data boundary, and hidden files other than .git/.harness are searched
mkdir -p $W4/.github $W4/.git
for f in .github/ci.yml .git/config .envrc plain.txt; do echo needle > $W4/$f; done
check "search covers hidden files but not .git" "$($H exec --workspace $W4 'local rows = search("needle", {literal=true}); local files = {}; for _, r in rows do table.insert(files, r.file) end; table.sort(files); print(table.concat(files, ","))' | sed -n 1p)" ".envrc,.github/ci.yml,plain.txt"
check "capped search rows are data" "$($H exec --workspace $W4 'local rows, more = search("needle", {max=1}); durable.set("rows", rows); print(#rows, more)' | sed -n 1p)" "1	true"

# 4. vars() lists session variables
check "vars" "$($H exec --workspace $W $'x = 41\nvars()' | head -1)" "=> <table: 1 keys> {x=41}"

# 4b. cut output keeps its start and end; the next chunk reads the middle from _cut; sh keeps the end of a huge stream
check "a cut temporary is readable next chunk" "$($H exec --workspace $W 'print(sh("seq 1 3000").stdout)' 'print(_cut[1]:find("\n1500\n", 1, true) ~= nil, vars()._cut == nil)' | sed -n '/^true/p')" "true	true"
check "sh keeps the end of a stream over 1 MiB" "$($H exec --workspace $W 'local r = sh("seq 1 400000; echo FINAL-LINE"); print(r.truncated, r.stdout:sub(-11) == "FINAL-LINE\n", sh("echo hi").truncated)' | head -1)" "true	true	false"

# 4c. sh forks for an unprivileged user who already owns many threads (a desktop session: browsers, editors). The
# suites run as root, which the kernel's per-user process limit exempts, so this runs as nobody when it can.
as_user=(); [ "$(id -u)" = 0 ] && command -v setpriv >/dev/null && as_user=(setpriv --reuid=65534 --regid=65534 --clear-groups)
U=$(mktemp -d); chmod 755 "$U"; mkdir "$U/ws" "$U/config" "$U/state"; [ ${#as_user[@]} -gt 0 ] && chown 65534:65534 "$U/ws" "$U/config" "$U/state"
"${as_user[@]}" python3 -c '
import threading, time
for _ in range(400): threading.Thread(target=time.sleep, args=(60,), daemon=True).start()
print("ready", flush=True); time.sleep(60)' > "$U/ready" & THREADS=$!
for _ in $(seq 100); do grep -q ready "$U/ready" 2>/dev/null && break; sleep 0.05; done
check "sh forks for a user who owns hundreds of threads" "$(HOME=$U/config XDG_CONFIG_HOME=$U/config XDG_STATE_HOME=$U/state "${as_user[@]}" timeout 60 $H exec --workspace "$U/ws" 'local r = sh("echo a | cat"); print(r.stdout == "a\n", r.code)' 2>&1 | head -1)" "true	0"
kill $THREADS; rm -rf "$U"

# 5. packages: declared capabilities, trust
P=$W/.harness/pkgs/mypkg; mkdir -p $P/tests
printf '[package]\nname = "mypkg"\nversion = "0.1.0"\n[capabilities]\nnet = ["example.com"]\n' > $P/hx.toml
echo 'return { hello = function() return "hi" end }' > $P/init.luau
echo 'test.case("loads", function() test.eq(require("mypkg").hello(), "hi") end)' > $P/tests/t.luau
check "an unknown capability is a load error, not a silent grant of nothing" "$($H test $P --workspace $W 2>&1 | grep -c 'unknown capability net.example.com')" "1"
printf '[package]\nname = "mypkg"\nversion = "0.1.0"\n[capabilities]\nfs = ["read"]\n' > $P/hx.toml
check "hx test loads the package as it is on disk" "$($H test $P --workspace $W | tail -1)" "1 passed, 0 failed"
HX_TRUST= $H trust --workspace $W --yes >/dev/null
echo 'return { hello = function() return "evil" end }' > $P/init.luau
check "a tampered package untrusts the workspace" "$(HX_TRUST= $H exec --workspace $W 'print(1)' 2>&1 | grep -c '.harness/pkgs/mypkg (changed)')" "1"
# a failed lock check must fail on EVERY require, not just the first
mkdir -p $W/probe/tests
cat > $W/probe/tests/t.luau <<'LUA'
test.case("failed require never enters a cache",function()
 local a=pcall(require,"mypkg");local b=pcall(require,"mypkg")
 test.ok(not a and not b)
end)
LUA
check "tampered package refused on second require" "$(HX_TRUST= $H test $W/probe --workspace $W 2>/dev/null | tail -1)" "1 passed, 0 failed"

# 6. hooks can deny, subagents run, usage aggregates (scripted model)
W2=$T/ws2; mkdir -p $W2/.harness; echo hi > $W2/readme.txt
cat > $W2/.harness/init.luau <<'LUA'
require("std/recheck").enabled = false -- this scenario tests hooks and shared usage
require("std/hooks").on("before_run", function(p)
	if p.code:find("rm -rf", 1, true) then return { deny = "no rm -rf here" } end
end)
LUA
MOCK_REPLIES='["```luau\nsh(\"rm -rf /tmp/zzz-hx\")\n```","```luau\nlocal r = agent(\"sub\")\nprint(\"sub said: \" .. r.answer)\n```","```luau\nprint(1)\n```","SUB-ANSWER","All done."]' serve MP python3 scripts/mock_llm.py 0 || exit 1; MOCK=$SERVED
echo go > $T/p.txt
OPENROUTER_API_KEY=x $H run --workspace $W2 --prompt-file $T/p.txt --model m --base-url http://127.0.0.1:$MP --key-env OPENROUTER_API_KEY --trace $T/trace.txt > $T/rep.json
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
MOCK_REPLIES='["```luau\nprint(1)\n```","First answer.","Checked: final answer."]' serve MP python3 scripts/mock_llm.py 0 || exit 1; MOCK=$SERVED
OPENROUTER_API_KEY=x $H run --workspace $W4 --prompt-file $T/p.txt --model m --base-url http://127.0.0.1:$MP --key-env OPENROUTER_API_KEY --trace $T/trace4.txt > $T/rep4.json
check "before_finish continues once" "$(python3 -c "import json;r=json.load(open('$T/rep4.json'));print(r['llm_calls'], r['final_answer'])")" "3 Checked: final answer."

# 6d. opt-in recheck sends the model back exactly once
W6=$T/ws6; mkdir -p $W6/.harness
echo 'require("std/recheck").enabled = true' > $W6/.harness/init.luau
kill $MOCK 2>/dev/null; wait $MOCK 2>/dev/null
MOCK_REPLIES='["```luau\nprint(1)\n```","First answer.","Checked: final answer."]' serve MP python3 scripts/mock_llm.py 0 || exit 1; MOCK=$SERVED
OPENROUTER_API_KEY=x $H run --workspace $W6 --prompt-file $T/p.txt --model m --base-url http://127.0.0.1:$MP --key-env OPENROUTER_API_KEY --trace $T/trace6.txt > $T/rep6.json
check "opt-in recheck continues once" "$(python3 -c "import json;r=json.load(open('$T/rep6.json'));print(r['llm_calls'], r['final_answer'])") $(grep -c 'state the root cause you fixed' $T/trace6.txt)" "3 Checked: final answer. 1"

W7=$T/ws7; mkdir -p $W7
kill $MOCK 2>/dev/null; wait $MOCK 2>/dev/null
MOCK_REPLIES='["```luau\nprint(1)\n```","First answer.","Checked: final answer."]' serve MP python3 scripts/mock_llm.py 0 || exit 1; MOCK=$SERVED
OPENROUTER_API_KEY=x $H run --workspace $W7 --prompt-file $T/p.txt --model m --base-url http://127.0.0.1:$MP --key-env OPENROUTER_API_KEY --trace $T/trace7.txt > $T/rep7.json
check "default accepts the first final answer" "$(python3 -c "import json;r=json.load(open('$T/rep7.json'));print(r['llm_calls'], r['final_answer'])") $(grep -c 'state the root cause you fixed' $T/trace7.txt)" "2 First answer. 0"

# 6c. Lost or malformed generations are not automatically retried (and retain reservations).
HX_CHECK_FILTER="lost model response" python3 scripts/redesign_checks.py >/dev/null
check "lost generations are not retried" "$?" "0"

# 7. a live run keeps its frozen source and session state
W3=$T/ws3; mkdir -p $W3/pk/tests $W3/.harness/override/std
cat > $W3/pk/tests/reload_test.luau <<'LUA'
test.case("a run keeps its frozen printer", function()
	test.eq(__format(5), "5")
	-- the override is user-owned (protected even under --yes): signal the script, which writes it like a user would
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

# 9. Exits end the session: a failing `hx test`, a signal stop and a closed stdout still write session_end and remove
# the private temp directory; a closed stdout is a quiet exit, not a panic.
W5=$T/ws5; mkdir -p $W5/pkg/tests $T/tmp5; echo 'assert(false, "nope")' > $W5/pkg/tests/fail_test.luau
ended() { python3 -c '
import json, sys, glob
print(all(json.loads(open(f).readlines()[-1])["kind"] == "session_end" for f in glob.glob(sys.argv[1] + "/*.jsonl")))' "$(logdir $1)"; }
(cd $W5 && TMPDIR=$T/tmp5 $H test pkg >/dev/null 2>&1); status=$?
check "a failing hx test exits 1 and ends its session" "$status $(ended $W5) $(ls $T/tmp5 | wc -l)" "1 True 0"
rm -rf $W5/.harness
TMPDIR=$T/tmp5 $H exec --workspace $W5 'print("up")' 'local t = os.clock(); while os.clock() - t < 20 do end' > $T/up.txt 2>&1 & HX=$!
for _ in $(seq 100); do [ -s $T/up.txt ] && break; sleep 0.1; done
kill -TERM $HX; wait $HX; status=$?
check "a signal stop ends the session" "$status $(ended $W5) $(ls $T/tmp5 | wc -l)" "143 True 0"
rm -rf $W5/.harness
out=$(TMPDIR=$T/tmp5 $H exec --workspace $W5 'print(1)' 'local t = os.clock(); while os.clock() - t < 1 do end; print(2)' 2>$T/err5 | head -1; echo " ${PIPESTATUS[0]}")
check "a closed stdout is a quiet exit" "$(echo $out) $(wc -c < $T/err5) $(ended $W5) $(ls $T/tmp5 | wc -l)" "1 141 0 True 0"

# 10. Nothing a command starts outlives it, and nothing hx starts outlives hx. escape.sh NAME leaves a process behind
# outside the command's process group (detached, double-forked, nohup'd, with a scrubbed environment, renamed to
# confuse a /proc parser, or ignoring SIGTERM) and writes its pid to NAME.pid once it is in place.
W6=$T/ws6; mkdir -p $W6
cat > $W6/escape.sh <<'SH'
mkfifo $1.fifo
in_place="echo \$\$ > $1.pid; echo > $1.fifo"
case $1 in
  setsid*) setsid sh -c "$in_place; exec sleep 300" & ;;
  doublefork) (setsid sh -c "sh -c '$in_place; exec sleep 300' &" &) ;;
  nohup) nohup setsid sh -c "$in_place; exec sleep 300" >/dev/null 2>&1 & ;;
  envi) env -i /usr/bin/setsid /bin/sh -c "$in_place; exec sleep 300" & ;;
  renamed) setsid sh -c "$in_place; exec python3 -c 'import ctypes, time; ctypes.CDLL(None).prctl(15, b\"x) S 1 1 1\", 0, 0, 0); time.sleep(300)'" & ;;
  noterm) setsid sh -c "trap '' TERM HUP INT; $in_place; while :; do sleep 1; done" & ;;
esac
read < $1.fifo
SH
# alive PID: running (a zombie is not: after hx exits, this container's init may not reap it)
alive() { case "$(ps -o stat= -p "$1" 2>/dev/null)" in ""|Z*) echo gone;; *) echo alive;; esac; }
for m in setsid doublefork nohup envi renamed noterm; do
  out=$($H exec --workspace $W6 "local _ = sh('bash escape.sh $m')" "print(sh('[ -d /proc/\$(cat $m.pid) ] && echo alive || echo gone').stdout)" 2>&1 | grep -xE 'alive|gone')
  check "a $m process is gone when sh returns, and after hx exits" "$out $(alive "$(cat $W6/$m.pid)")" "gone gone"
done
out=$($H exec --workspace $W6 "print(sh('bash escape.sh setsid2; exec sleep 300', {timeout = 1}).timed_out)" "print(sh('[ -d /proc/\$(cat setsid2.pid) ] && echo alive || echo gone').stdout)" 2>&1 | grep -xE 'true|false|alive|gone' | tr '\n' ' ')
check "...and after sh times out" "$out" "true gone "
$H exec --workspace $W6 "sh('bash escape.sh setsid3; exec sleep 300')" >/dev/null 2>&1 & HX=$!
for _ in $(seq 100); do [ -s $W6/setsid3.pid ] && break; sleep 0.1; done
kill -TERM $HX; wait $HX
check "...and when hx is stopped by a signal" "$? $(alive "$(cat $W6/setsid3.pid)")" "143 gone"

finish
