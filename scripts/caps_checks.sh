#!/usr/bin/env bash
# Runtime capability enforcement: a project package only gets the primitives it requested AND the user granted.
set -u
cd "$(dirname "$0")/.."
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
H=$PWD/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); trap 'rm -rf "$T"' EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
W=$T/ws; mkdir -p $W/.harness/pkgs/reader/tests $W/.harness/pkgs/runner/tests; echo hello > $W/a.txt

mk() { # name, capabilities toml
  P=$W/.harness/pkgs/$1
  printf '[package]\nname = "%s"\nversion = "0.1.0"\n[capabilities]\n%s\n' "$1" "$2" > $P/hx.toml
  cat > $P/init.luau <<'LUA'
local M = {}
function M.read_ok() return read("a.txt") end
function M.write() return write("x.txt", "pwn") end
function M.edit() return edit("a.txt", "hello", "pwn") end
function M.sh() return sh("echo hi > y.txt; echo done").stdout end
function M.search() return #search("hello") end
function M.agent() return agent("x") end
function M.llm() return hx.llm({}) end
function M.getfenv() return getfenv(0) end
function M.setfenv() return setfenv(1, {}) end
function M.loadstring() return loadstring("return 1")() end
function M.globals() return _G.sh end
function M.poison() string.rep = nil end
function M.require_std() return type(require("std/printer").top) end
function M.getmeta() return getmetatable(M) end
return M
LUA
}

mk reader 'fs = ["read"]'
cat > $W/.harness/pkgs/reader/tests/t.luau <<'LUA'
local m = require("reader")
local function denied(f) local ok, e = pcall(f) return (not ok) and tostring(e):find("capability_denied", 1, true) ~= nil end
local function absent(f) local ok = pcall(f) return not ok end
test.case("granted read works", function() test.eq(m.read_ok(), "hello\n"); test.eq(m.search(), 1) end)
test.case("write/edit/sh/agent/llm are denied with a clear error", function()
	test.ok(denied(m.write), "write"); test.ok(denied(m.edit), "edit"); test.ok(denied(m.sh), "sh"); test.ok(denied(m.agent), "agent")
	test.ok(absent(m.llm), "hx.llm")
end)
test.case("nothing was written or executed", function()
	test.eq(sh("test -e x.txt -o -e y.txt; echo $?").stdout, "1\n")
	test.eq(read("a.txt"), "hello\n")
end)
test.case("escape hatches do not exist", function()
	test.ok(absent(m.getfenv), "getfenv"); test.ok(absent(m.setfenv), "setfenv"); test.ok(absent(m.loadstring), "loadstring")
	test.ok(absent(m.globals), "_G")
end)
test.case("builtin libraries are read-only", function() test.ok(absent(m.poison), "string.rep = nil"); test.eq(type(string.rep), "function") end)
test.case("std modules are reachable (the harness API)", function() test.eq(m.require_std(), "function") end)
LUA
mk runner 'fs = ["read"]
proc = ["sh"]'
cat > $W/.harness/pkgs/runner/tests/t.luau <<'LUA'
local m = require("runner")
test.case("proc.sh granted: sh works (inside the sandbox)", function() test.eq(m.sh(), "done\n") end)
test.case("fs.write still denied", function() local ok, e = pcall(m.write) test.ok(not ok and tostring(e):find("capability_denied", 1, true)) end)
LUA
check "unapproved packages are refused" "$($H test $W/.harness/pkgs/reader --workspace $W 2>&1 | grep -c 'is not locked')" "1"
$H approve reader --workspace $W >/dev/null; $H approve runner --workspace $W >/dev/null
out=$($H test $W/.harness/pkgs/reader --workspace $W 2>&1); check "reader package: all capability and escape tests pass" "$(echo "$out" | tail -1)" "6 passed, 0 failed"; echo "$out" | grep FAIL
out=$($H test $W/.harness/pkgs/runner --workspace $W 2>&1); check "runner package: proc.sh granted, fs.write denied" "$(echo "$out" | tail -1)" "2 passed, 0 failed"; echo "$out" | grep FAIL

# a package that asks for more than was granted is refused at load
printf '[package]\nname = "greedy"\nversion = "0.1.0"\n[capabilities]\nfs = ["read", "write"]\nproc = ["sh"]\n' > /dev/null
mkdir -p $W/.harness/pkgs/greedy/tests; cp $W/.harness/pkgs/reader/init.luau $W/.harness/pkgs/greedy/init.luau
printf '[package]\nname = "greedy"\nversion = "0.1.0"\n[capabilities]\nfs = ["read"]\n' > $W/.harness/pkgs/greedy/hx.toml; echo 'test.case("x", function() require("greedy") end)' > $W/.harness/pkgs/greedy/tests/t.luau
$H approve greedy --workspace $W >/dev/null
printf '[package]\nname = "greedy"\nversion = "0.1.0"\n[capabilities]\nfs = ["read", "write"]\nproc = ["sh"]\n' > $W/.harness/pkgs/greedy/hx.toml
check "widening capabilities after approval is refused (hash changed)" "$($H test $W/.harness/pkgs/greedy --workspace $W 2>&1 | grep -c 'changed since it was approved')" "1"

# the model's own session has none of the escape hatches either
out=$($H exec --workspace $W 'print(getfenv, setfenv, loadstring, require)' | head -1)
check "model session: getfenv/setfenv/loadstring/require are gone" "$out" "nil	nil	nil	nil"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
