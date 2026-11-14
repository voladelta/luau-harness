#!/usr/bin/env bash
# File-tool policy and shell confinement checks. Run after `cargo build --release`.
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
W=$(mktemp -d); W=$(cd "$W" && pwd -P)
OUT=$(mktemp -d)
S1=$OUT/sentinel.txt; ESC=$OUT/outside.txt; ESC2=$OUT/dotdot.txt; ESC3=$OUT/viadir.txt
cleanup() { rm -rf "$W" "$OUT"; pkill -f "sleep 987" 2>/dev/null; }
run() { $H exec --workspace "$W" "$@" 2>&1; }

echo SECRET > $S1; echo ORIGINAL > $W/inside.txt
mkdir -p $W/.harness/override/std; echo 'return 1' > $W/.harness/override/std/marker.luau; echo '[grant.x]' > $W/.harness/grants.toml; mkdir -p $W/.agents/skills/s; echo x > $W/.agents/skills/s/SKILL.md; echo 'return 1' > $W/.harness/init.luau

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
for f in .harness/grants.toml .agents/skills/s/SKILL.md .agents/skills/new/SKILL.md .harness/init.luau .harness/override/std/marker.luau; do
  out=$(run "print(pcall(write, \"$f\", \"x\"))" | head -1)
  check "write() cannot modify $f" "$(echo "$out" | grep -c 'protected')" "1"
done
out=$(run 'print(pcall(edit, ".harness/grants.toml", "grant", "grunt"))' | head -1)
check "edit() cannot modify grants" "$(echo "$out" | grep -c 'protected')" "1"
# --- environment
out=$(run 'print(sh("env").stdout)')
check "no API keys in sh env" "$(echo "$out" | grep -ci 'API_KEY\|TOKEN\|SECRET_')" "0"
check "HOME is the run's temp dir, not the real home" "$(echo "$out" | grep '^HOME=' | grep -c "$HOME\$")" "0"

# --- shell confinement: Landlock applies the file policy to shell commands
# hx's own files (the workspace's secrets, its journals and autoresearch runs) are outside the workspace and every root.
SEC=$($H paths --workspace "$W" | sed -n 's/^secrets: //p'); AR=$($H paths --workspace "$W" | sed -n 's/^state: //p')/tool-run
mkdir -p "$(dirname "$SEC")" "$AR/run"; echo 'K = "CREDENTIAL"' > "$SEC"; echo SEALED-MARK > "$AR/run/held.txt"; echo PUBLIC > $W/README.md
out=$(run "local s, r = sh('cat $SEC'), sh('cat README.md') print(s.code ~= 0, s.stdout == '', r.stdout)" | head -1)
check "confined shell cannot read the workspace secrets but reads README.md" "$(echo "$out" | tr '\t' '|')" "true|true|PUBLIC"
out=$(run 'local r = sh("echo hi > new.txt; cat new.txt; mkdir -p venv/bin && echo ok > venv/bin/x && cat venv/bin/x; git init -q fresh && echo git-ok") print(r.stdout:gsub("\n", " "))' | head -1)
check "a command reads the files it creates in a workspace that has secrets" "$out" "hi ok git-ok 	3"
out=$(run "print(sh('mv $SEC leak.txt').code ~= 0, sh('echo forged > $SEC').code ~= 0, sh('echo x > $AR/run/held.txt; mv $AR/run/held.txt out.txt').code ~= 0)" | head -1)
check "confined shell cannot move or overwrite the secrets or autoresearch state" "$(echo "$out" | tr '\t' '|')$(cat "$SEC" "$AR/run/held.txt" | tr '\n' '|')" "true|true|true"'K = "CREDENTIAL"|SEALED-MARK|'
out=$(run 'local a = pcall(read, "leak.txt") local b = pcall(read, "out.txt") print(a, b, #search("CREDENTIAL", {literal = true}), sh("cat leak.txt out.txt 2>/dev/null").stdout == "")' | head -1)
check "nothing of it is reachable in the next run" "$(echo "$out" | tr '\t' '|')" "false|false|0|true"
out=$($H exec --workspace "$W" --yes "print(pcall(read, '$SEC'), pcall(read, '$AR/run/held.txt'), pcall(write, '$AR/run/held.txt', 'x'))" 2>&1 | head -1)
check "file tools refuse hx's own files, even with --yes" "$(echo "$out" | cut -f1-3 | tr '\t' '|')" "false|false|false"
rm -rf "$W/leak.txt" "$W/out.txt" "$W/new.txt" "$W/venv" "$W/fresh" "$AR"
KEYS=$OUT/home/.ssh; mkdir -p $KEYS; echo PRIVATE-KEY > $KEYS/id_rsa
out=$($H exec --workspace "$W" --yes "print(pcall(sh, 'cat id_rsa', {cwd = '$KEYS'}))" 2>&1 </dev/null | head -1)
check "--yes never grants a directory: no shell command in ~/.ssh" "$(echo "$out" | grep -c PRIVATE-KEY)$(echo "$out" | grep -c 'approval_required')" "01"
out=$(run 'print(pcall(read, "/dev/tty"), sh("echo x > /dev/tty; ls /dev/pts").code ~= 0, sh("head -c1 /dev/urandom >/dev/null").code)' | head -1)
check "no terminal access for file tools or confined commands" "$(echo "$out" | tr '\t' '|')" "false|true|0"
REALHOME=$(mktemp -d); mkdir -p $REALHOME/.config/hx; echo ORIG > $REALHOME/.config/hx/trust.toml
HOME=$REALHOME $H exec --workspace "$W" --yes "write('$REALHOME/notes.txt', 'n\\n') sh('echo PWNED > $REALHOME/.config/hx/trust.toml; echo PWNED > $REALHOME/other.txt')" >/dev/null 2>&1
check "--yes write grant covers the file, not its parent tree" "$(cat $REALHOME/notes.txt $REALHOME/.config/hx/trust.toml; [ -e $REALHOME/other.txt ] && echo WROTE)" "n
ORIG"
# A write that names a directory is no single file: it must not become a tree a later command can write.
for target in / "$REALHOME"; do
  HOME=$REALHOME $H exec --workspace "$W" --yes "pcall(write, '$target', 'x') sh('echo PWNED > $REALHOME/.config/hx/trust.toml; echo PWNED > $REALHOME/other.txt')" >/dev/null 2>&1
done
check "--yes write on a directory grants nothing" "$(cat $REALHOME/.config/hx/trust.toml; [ -e $REALHOME/other.txt ] && echo WROTE)" "ORIG"
out=$(HOME=$REALHOME $H exec --workspace "$W" --yes "print(pcall(write, '$REALHOME/.bashrc', 'x'), pcall(write, '$REALHOME/.config/systemd/user/x.service', 'x'))" 2>&1 </dev/null | head -1)
check "--yes does not write hidden files outside the roots" "$(echo "$out" | cut -f1,2 | tr '\t' '|')$(echo "$out" | grep -c 'does not grant')$([ -e $REALHOME/.bashrc ] && echo WROTE)" "false|false1"
rm -rf $REALHOME
out=$(run "local r = sh('cat $S1; echo written > $ESC') print(r.code ~= 0, r.stdout == '')" | head -1)
check "confined shell cannot read or write beyond the roots" "$(echo "$out" | tr '\t' '|')$([ -e $ESC ] && echo WROTE)" "true|true"
out=$($H exec --workspace "$W" --yes "read(\"$S1\") print(sh('cat $S1').stdout)" 2>&1 | head -1)
check "a path grant approved mid-run applies to later shell commands" "$out" "SECRET"
rm -f "$SEC"

# --- approval classes: a program the classifier cannot read needs approval; everyday commands do not
pad=$(printf -- '-u A %.0s' $(seq 40))
for cmd in "env $pad touch padded.txt" "\"\$(echo touch)\" substituted.txt" "tou\$'c'h quoted.txt"; do
  out=$(run "print(pcall(sh, [[$cmd]]))" | head -1)
  check "unreadable program needs approval and does not run: ${cmd:0:24}" "$(echo "$out" | grep -c 'approval_required')$(ls $W/*ed.txt 2>/dev/null)" "1"
done
for cmd in "time -p git push origin main" "env -S 'git push origin main'" "env --split-string='git push --force'"; do
  out=$(run "print(pcall(sh, [[$cmd]]))" | head -1)
  check "a command behind time -p or env -S needs approval: ${cmd:0:22}" "$(echo "$out" | grep -c 'approval_required')" "1"
done
out=$(run "print(sh([[find . -type f \\( -name '*.rs' -o -name '*.py' -o -name '*.md' -o -name '*.toml' -o -name '*.lua' -o -name '*.luau' -o -name '*.sh' -o -name '*.txt' \\) -not -path './target/*' -not -path './.git/*' -exec grep -l TODO {} +]]).code ~= nil)" | head -1)
check "a long read-only find -exec runs unasked" "$out" "true"
out=$(run 'local a, b, c = sh("command -v sh"), sh("env RUST_BACKTRACE=1 timeout 60 true"), sh("for f in *.txt; do wc -l \"$f\" >/dev/null; done")
print(a.code, b.code, c.code)' | head -1)
check "command -v, wrapped and looping commands run unasked" "$(echo "$out" | tr '\t' '|')" "0|0|0"

# --- timeout kills the whole process group
out=$(run 'local r = sh("sleep 987 & sleep 987", {timeout = 1}) print(r.timed_out)')
check "timeout kills the whole process group" "$(echo "$out" | head -1)$(sleep 1; pgrep -f 'sleep 987' | wc -l | tr -d ' ')" "true0"

# --- without Landlock nothing that could start a child runs, unless the owner launches it with --unconfined
nolandlock() { HX_TEST_NO_LANDLOCK=1 "$@" 2>&1; }
journals() { ls "$(logdir "$W")"/*.jsonl 2>/dev/null | wc -l; }
before=$(journals)
out=$(nolandlock $H exec --workspace "$W" "write('ran.txt', 'x')"); rc=$?
check "without Landlock hx exec is refused before anything runs" "$rc $(echo "$out" | grep -c 'Landlock is unavailable.*pass --unconfined') $([ -e $W/ran.txt ] && echo RAN) $(journals)" "1 1  $before"
mkdir -p $W/pk/tests; echo 'test.case("t", function() end)' > $W/pk/tests/t_test.luau
out=$(cd $W && nolandlock $H test pk); rc=$?
check "...and so is hx test" "$rc $(echo "$out" | grep -c 'Landlock is unavailable')" "1 1"
out=$(cd $W && nolandlock $H test pk --unconfined | tail -1)
check "hx test --unconfined runs" "$out" "1 passed, 0 failed"
out=$(nolandlock $H exec --workspace "$W" --unconfined "print(sh('cat $S1; echo written > $ESC').stdout)" | head -1)
check "hx exec --unconfined runs shell commands unconfined" "$out$(cat "$ESC")" "SECRETwritten"
check "...and the journal says so" "$(grep -h '"kind":"session"' "$(ls -t "$(logdir "$W")"/*.jsonl | head -1)" | grep -c '"confinement":"off: disabled by HX_TEST_NO_LANDLOCK"')" "1"
rm -rf "$ESC" $W/pk
printf 'unconfined = true\n' > $W/.harness/policy.toml
out=$(nolandlock $H exec --workspace "$W" 'print(1)'); rc=$?
check "policy.toml cannot turn confinement off" "$rc $(echo "$out" | grep -c 'Landlock is unavailable') $($H exec --workspace "$W" 'print(1)' 2>&1 | grep -c 'bad .harness/policy.toml')" "1 1 1"
rm $W/.harness/policy.toml

finish
