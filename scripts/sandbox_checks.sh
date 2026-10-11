#!/usr/bin/env bash
# macOS sandbox escape suite. Everything the model can reach (sh, read, write, edit, search) must stay inside policy.
# Uses sentinel files in $HOME and a bounded process spawner (NOT a real fork bomb). Run after `cargo build --release`.
set -u
cd "$(dirname "$0")/.."
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
H=$PWD/target/release/hx
W=$(mktemp -d); W=$(cd "$W" && pwd -P)
S1=$HOME/hx-sandbox-sentinel.txt; ESC=$HOME/hx-escape-target.txt; ESC2=$HOME/hx-escape-dotdot.txt; ESC3=$HOME/hx-escape-viadir.txt; ESC4=/tmp/hx-escape-tmp.txt
cleanup() { rm -rf "$W" "$S1" "$ESC" "$ESC2" "$ESC3" "$ESC4"; pkill -f "sleep 987" 2>/dev/null; pkill -f "http.server $PORT" 2>/dev/null; }
HTTP=; PORT=$((20000 + RANDOM % 20000)); trap cleanup EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
run() { $H exec --workspace "$W" "$@" 2>&1; }

echo SECRET > $S1; echo ORIGINAL > $W/inside.txt
mkdir -p $W/.harness/override/std; echo 'return 1' > $W/.harness/override/std/marker.luau; echo '[grant.x]' > $W/.harness/grants.toml; echo lock > $W/hx.lock; echo 'return 1' > $W/.harness/init.luau

# --- normal work still works
out=$(run 'write("sub/dir/new.txt", "hello")
edit("inside.txt", "ORIGINAL", "CHANGED")
local r = sh("python3 -c \"print(6*7)\"; git init -q . && echo git-ok")
print(read("sub/dir/new.txt"), read("inside.txt"):gsub("\n", ""), r.stdout:gsub("\n", " "))')
check "normal work (write/edit/python/git)" "$(echo "$out" | head -1 | tr '\t' '|')" "hello|CHANGED|42 git-ok |2"

# --- writes outside the workspace
run 'sh("echo x > $HOME/hx-escape-target.txt")' >/dev/null
check "sh cannot write into \$HOME" "$([ -e $ESC ] && echo WROTE || echo blocked)" "blocked"
run 'sh("echo x > /tmp/hx-escape-tmp.txt")' >/dev/null
check "sh cannot write to /tmp" "$([ -e $ESC4 ] && echo WROTE || echo blocked)" "blocked"
out=$(run 'print(pcall(write, "../hx-escape-dotdot.txt", "x"))' | head -1)
check "write() rejects ../ escape" "$(echo "$out" | grep -c 'outside the allowed roots')$([ -e $ESC2 ] && echo WROTE)" "1"
out=$(run "print(pcall(write, \"$ESC\", \"x\"))" | head -1)
check "write() rejects absolute path outside" "$(echo "$out" | grep -c 'outside the allowed roots')$([ -e $ESC ] && echo WROTE)" "1"

# --- reads outside
out=$(run 'print(sh("cat $HOME/hx-sandbox-sentinel.txt 2>&1").stdout)')
check "sh cannot read \$HOME sentinel" "$(echo "$out" | grep -c SECRET)" "0"
out=$(run "print(pcall(read, \"$S1\"))" | head -1)
check "read() rejects path outside roots" "$(echo "$out" | grep -c 'outside the allowed roots')$(echo "$out" | grep -c SECRET)" "10"

# --- credential files stay unreadable even inside a read root (sh, read, search alike)
mkdir -p $W/creds; for f in credentials credentials.toml .netrc .npmrc .pypirc id_rsa id_ed25519; do echo "CRED-$f" > "$W/creds/$f"; done
out=$(run '
local leaked = {}
for _, f in {"credentials","credentials.toml",".netrc",".npmrc",".pypirc","id_rsa","id_ed25519"} do
	if sh("cat creds/" .. f).stdout:find("CRED") then table.insert(leaked, "sh:" .. f) end
	local ok, t = pcall(read, "creds/" .. f)
	if ok and t:find("CRED") then table.insert(leaked, "read:" .. f) end
end
if #search("CRED", {path = "creds", literal = true}) > 0 then table.insert(leaked, "search") end
for _, f in {"credentials.toml", ".npmrc"} do
	-- edit() reads the file: its result must not tell whether a guessed string is in it
	if pcall(edit, "creds/" .. f, "CRED", "CRED") then table.insert(leaked, "edit:" .. f) end
end
print("leaked=" .. table.concat(leaked, ","))' | head -1)
check "credential files: unreadable via sh, read(), search() and edit()" "$out" "leaked="

# --- symlink tricks
ln -s $S1 $W/link-file; ln -s $HOME $W/link-dir
out=$(run 'print(pcall(read, "link-file"))' | head -1)
check "read() through symlink to outside file blocked" "$(echo "$out" | grep -c SECRET)" "0"
out=$(run 'print(sh("cat link-file 2>&1").stdout)')
check "sh cat through symlink blocked" "$(echo "$out" | grep -c SECRET)" "0"
out=$(run 'print(pcall(write, "link-file", "PWNED"))' | head -1)
check "write() through symlink refused" "$(echo "$out" | grep -c 'symlink\|outside')$(grep -c PWNED $S1)" "10"
run 'sh("echo PWNED >> link-file")' >/dev/null
check "sh append through symlink blocked" "$(grep -c PWNED $S1)" "0"
out=$(run 'print(pcall(write, "link-dir/hx-escape-viadir.txt", "x"))' | head -1)
check "write() through symlinked dir refused" "$(echo "$out" | grep -c 'outside')$([ -e $ESC3 ] && echo WROTE)" "1"
run 'sh("echo x > link-dir/hx-escape-viadir.txt")' >/dev/null
check "sh write through symlinked dir blocked" "$([ -e $ESC3 ] && echo WROTE || echo blocked)" "blocked"
ln -s /nonexistent-hx-dir/evil $W/dangling
out=$(run 'print(pcall(write, "dangling", "x"))' | head -1)
check "write() through dangling symlink refused" "$(echo "$out" | grep -c 'symlink')" "1"
out=$(run 'local r = search("SECRET", {path = "link-dir"}) print(#r)' 2>&1 | head -1)
check "search through symlinked dir finds nothing" "$(echo "$out" | grep -c '^0$\|outside')" "1"

# --- protected harness configuration
for f in .harness/grants.toml hx.lock .harness/init.luau .harness/override/std/marker.luau; do
  out=$(run "print(pcall(write, \"$f\", \"x\"))" | head -1)
  check "write() cannot modify $f" "$(echo "$out" | grep -c 'protected')" "1"
  run "sh(\"echo PWNED > $f\")" >/dev/null
  check "sh cannot modify $f" "$(grep -c PWNED $W/$f 2>/dev/null)" "0"
done
out=$(run 'print(pcall(edit, ".harness/grants.toml", "grant", "grunt"))' | head -1)
check "edit() cannot modify grants" "$(echo "$out" | grep -c 'protected')" "1"
run 'sh("rm -rf .harness; rm -f hx.lock")' >/dev/null
check "sh cannot delete .harness or hx.lock" "$([ -f $W/.harness/grants.toml ] && [ -f $W/hx.lock ] && echo kept || echo DELETED)" "kept"

# --- environment
out=$(run 'print(sh("env").stdout)')
check "no API keys in sh env" "$(echo "$out" | grep -ci 'API_KEY\|TOKEN\|SECRET_')" "0"
check "HOME is the run's temp dir, not the real home" "$(echo "$out" | grep '^HOME=' | grep -c "$HOME\$")" "0"

# --- network
out=$(run 'local r = sh("curl -s -m 4 -o /dev/null -w %{http_code} https://example.com; echo \" exit=$?\"") print(r.stdout)')
check "no network by default (curl)" "$(echo "$out" | grep -c 'exit=0')" "0"
out=$(run 'local r = sh("python3 -c \"import socket;s=socket.socket();s.settimeout(3);s.connect((\\\"1.1.1.1\\\",443));print(\\\"CONNECTED\\\")\" 2>&1") print(r.stdout)')
check "no network by default (raw socket)" "$(echo "$out" | grep -c '^CONNECTED$')$(echo "$out" | grep -c PermissionError)" "01"
(cd $W && python3 -m http.server $PORT --bind 127.0.0.1 >/dev/null 2>&1) & HTTP=$!
sleep 1
out=$(run 'print(sh("curl -s -m 3 -o /dev/null -w %{http_code} http://127.0.0.1:'"$PORT"'/; echo \" exit=$?\"").stdout)')
check "localhost also blocked without a net grant" "$(echo "$out" | grep -c 'exit=0')" "0"
mkdir -p $W/.harness; printf '[net]\nallow = true\n' > $W/.harness/policy.toml
out=$(run 'print(sh("curl -s -m 3 -o /dev/null -w %{http_code} http://127.0.0.1:'"$PORT"'/; echo \" exit=$?\"").stdout)')
check "network works once granted in policy.toml" "$(echo "$out" | grep -c '200 exit=0')" "1"
rm $W/.harness/policy.toml
out=$($H exec --workspace $W --net 'print(sh("curl -s -m 3 -o /dev/null -w %{http_code} http://127.0.0.1:'"$PORT"'/; echo \" exit=$?\"").stdout)' 2>&1 | head -1)
check "--net grants network for one launch (no policy file needed)" "$(echo "$out" | grep -c '200 exit=0')" "1"

# --- processes: bounded spawner, timeout kills the whole group
out=$(run 'local r = sh([[python3 - <<"PY"
import os, time
kids = 0
for i in range(6000):
    try:
        pid = os.fork()
    except OSError:
        break
    if pid == 0:
        os.execvp("sleep", ["sleep", "987"])
    kids += 1
print("spawned", kids)
PY]], {timeout = 40}) print(r.stdout, r.timed_out)' | tr '\n' ' ')
n=$(echo "$out" | sed -n 's/.*spawned \([0-9]*\).*/\1/p')
check "process cap stops runaway spawning (<6000)" "$([ -n "$n" ] && [ "$n" -lt 6000 ] && echo capped || echo "UNBOUNDED($n)")" "capped"
sleep 1
check "no leftover spawned processes after sh returns" "$(pgrep -f 'sleep 987' | wc -l | tr -d ' ')" "0"
out=$(run 'local r = sh("sleep 987 & sleep 987", {timeout = 1}) print(r.timed_out)')
check "timeout kills the whole process group" "$(echo "$out" | head -1)$(sleep 1; pgrep -f 'sleep 987' | wc -l | tr -d ' ')" "true0"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
