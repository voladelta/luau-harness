#!/usr/bin/env bash
# File-tool policy and direct shell execution checks. Run after `cargo build --release`.
set -u
cd "$(dirname "$0")/.."
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
H=$PWD/target/release/hx
W=$(mktemp -d); W=$(cd "$W" && pwd -P)
OUT=$(mktemp -d)
S1=$OUT/sentinel.txt; ESC=$OUT/outside.txt; ESC2=$OUT/dotdot.txt; ESC3=$OUT/viadir.txt
cleanup() { rm -rf "$W" "$OUT"; pkill -f "sleep 987" 2>/dev/null; }
trap cleanup EXIT
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
out=$(run 'print(pcall(write, "../hx-escape-dotdot.txt", "x"))' | head -1)
check "write() rejects ../ escape" "$(echo "$out" | grep -c 'outside the allowed roots')$([ -e $ESC2 ] && echo WROTE)" "1"
out=$(run "print(pcall(write, \"$ESC\", \"x\"))" | head -1)
check "write() rejects absolute path outside" "$(echo "$out" | grep -c 'outside the allowed roots')$([ -e $ESC ] && echo WROTE)" "1"

# --- reads outside
out=$(run "print(pcall(read, \"$S1\"))" | head -1)
check "read() rejects path outside roots" "$(echo "$out" | grep -c 'outside the allowed roots')$(echo "$out" | grep -c SECRET)" "10"

# --- credential files stay unreadable even inside a read root (read, search, edit)
mkdir -p $W/creds; for f in credentials credentials.toml .netrc .npmrc .pypirc id_rsa id_ed25519; do echo "CRED-$f" > "$W/creds/$f"; done
out=$(run '
local leaked = {}
for _, f in {"credentials","credentials.toml",".netrc",".npmrc",".pypirc","id_rsa","id_ed25519"} do
	local ok, t = pcall(read, "creds/" .. f)
	if ok and t:find("CRED") then table.insert(leaked, "read:" .. f) end
end
if #search("CRED", {path = "creds", literal = true}) > 0 then table.insert(leaked, "search") end
for _, f in {"credentials.toml", ".npmrc"} do
	-- edit() reads the file: its result must not tell whether a guessed string is in it
	if pcall(edit, "creds/" .. f, "CRED", "CRED") then table.insert(leaked, "edit:" .. f) end
end
print("leaked=" .. table.concat(leaked, ","))' | head -1)
check "credential files: unreadable via read(), search() and edit()" "$out" "leaked="

# --- symlink tricks
ln -s $S1 $W/link-file; ln -s $OUT $W/link-dir
out=$(run 'print(pcall(read, "link-file"))' | head -1)
check "read() through symlink to outside file blocked" "$(echo "$out" | grep -c SECRET)" "0"
out=$(run 'print(pcall(write, "link-file", "PWNED"))' | head -1)
check "write() through symlink refused" "$(echo "$out" | grep -c 'symlink\|outside')$(grep -c PWNED $S1)" "10"
out=$(run 'print(pcall(write, "link-dir/hx-escape-viadir.txt", "x"))' | head -1)
check "write() through symlinked dir refused" "$(echo "$out" | grep -c 'outside')$([ -e $ESC3 ] && echo WROTE)" "1"
ln -s /nonexistent-hx-dir/evil $W/dangling
out=$(run 'print(pcall(write, "dangling", "x"))' | head -1)
check "write() through dangling symlink refused" "$(echo "$out" | grep -c 'symlink')" "1"
out=$(run 'local r = search("SECRET", {path = "link-dir"}) print(#r)' 2>&1 | head -1)
check "search through symlinked dir finds nothing" "$(echo "$out" | grep -c '^0$\|outside')" "1"

# --- protected harness configuration
for f in .harness/grants.toml hx.lock .harness/init.luau .harness/override/std/marker.luau; do
  out=$(run "print(pcall(write, \"$f\", \"x\"))" | head -1)
  check "write() cannot modify $f" "$(echo "$out" | grep -c 'protected')" "1"
done
out=$(run 'print(pcall(edit, ".harness/grants.toml", "grant", "grunt"))' | head -1)
check "edit() cannot modify grants" "$(echo "$out" | grep -c 'protected')" "1"
# --- environment
out=$(run 'print(sh("env").stdout)')
check "no API keys in sh env" "$(echo "$out" | grep -ci 'API_KEY\|TOKEN\|SECRET_')" "0"
check "HOME is the run's temp dir, not the real home" "$(echo "$out" | grep '^HOME=' | grep -c "$HOME\$")" "0"

# --- shell access is controlled by the outer environment, independently of file-tool roots
out=$(run "print(sh('cat $S1; echo written > $ESC').stdout)" | head -1)
check "shell runs directly and can read beyond file-tool roots" "$out" "SECRET"
check "shell writes beyond file-tool roots" "$(cat "$ESC")" "written"

# --- timeout kills the whole process group
out=$(run 'local r = sh("sleep 987 & sleep 987", {timeout = 1}) print(r.timed_out)')
check "timeout kills the whole process group" "$(echo "$out" | head -1)$(sleep 1; pgrep -f 'sleep 987' | wc -l | tr -d ' ')" "true0"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
