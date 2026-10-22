#!/usr/bin/env python3
"""Compaction history acceptance checks with local scripted providers only."""
import json
import redesign_checks as h

INIT = 'require("std/recheck").enabled=false\nrequire("std/compact").keep_ratio=0.001\n'
MARKER = "HISTORY_NEEDLE=8bd7e395"
FIRST = 'print(string.rep("x",2200).."HISTORY_".."NEEDLE=8bd7e395"..string.rep("y",2200))'
GET = '''local p=history({q="HISTORY_NEEDLE",limit=20}); local found=false
for _,e in p.entries do
 if e.role=="tool" then
  local chunks={}; local offset=0
  repeat
   local page=history({id=e.id,offset=offset})
   table.insert(chunks,page.text); offset=page.next_offset
  until offset==nil
  assert(table.concat(chunks):find("HISTORY_NEEDLE=8bd7e395",1,true))
  durable.set("old_entry",e.id); found=true
 end
end
assert(found); print("ARCHIVE_OK")'''


def is_summary(body):
    return "Write a context checkpoint" in h.body_text(body)


def script(repeat=False, empty=False):
    n = 0
    def answer(body, index):
        nonlocal n
        if is_summary(body):
            return "" if empty else "Only routine work remains. The old diagnostic text is omitted."
        n += 1
        if n == 1:
            return h.tool(FIRST)
        if n == 2:
            return {"code": 'print("tail")', "tokens": 23000}
        if n == 3:
            return {"code": 'assert(history().status=="unavailable");print("NO_ARCHIVE")' if empty else GET,
                    "tokens": 23000 if repeat else 10}
        if n == 4 and repeat:
            return h.tool('local p=history({id=durable.get("old_entry")});assert(p.text:find("HISTORY_NEEDLE=8bd7e395",1,true));print("OLD_ENTRY_SURVIVES")')
        return "done"
    return answer


def start(path, url, **kwargs):
    return h.start(path, url, "--context-window", "40000", "--max-output", "1000", **kwargs)


def archives(log):
    return [r for r in h.records(log) if r.get("archive")]


def roundtrip():
    path = h.workspace("history-roundtrip", INIT)
    with h.provider(script(repeat=True)) as (url, calls):
        report = json.loads(start(path, url).stdout)
        assert report["finished"] and report["luau_errors"] == 0, report
        log = h.journal(path)
        records = h.records(log)
        assert len(archives(log)) == 2, archives(log)
        summaries = [b for b in calls if is_summary(b)]
        assert MARKER in h.body_text(summaries[0])  # the summarizer replays exact messages, nothing truncated
        assert "OLD_ENTRY_SURVIVES" in h.body_text(calls[-1])
        assert any(r.get("tool") == "history" for r in records)
        count = len(calls)
        restored = json.loads(h.run(path, "run", "--resume", str(log)).stdout)
        assert restored["finished"] and len(calls) == count
        return archives(log)[0]["seq"]


def failed_summary():
    path = h.workspace("history-empty", INIT)
    with h.provider(script(empty=True)) as (url, calls):
        report = json.loads(start(path, url).stdout)
        assert report["finished"] and report["luau_errors"] == 0, report
        assert not archives(h.journal(path))


def crash_boundaries(commit):
    for number in (commit-1, commit):
        path = h.workspace(f"history-crash-{number}", INIT)
        normal = script()
        resumed = 0
        def answer(body, index):
            nonlocal resumed
            if "Recovered into a fresh Luau VM" in h.body_text(body):
                resumed += 1
                if resumed == 1:
                    status = "unavailable" if number < commit else "ok"
                    return h.tool('assert(history().status=="' + status + '");print("RECOVERY_HISTORY_OK")')
                return "done"
            return normal(body, index)
        with h.provider(answer) as (url, calls):
            crashed = start(path, url, env={"HX_TEST_CRASH_AFTER_RECORD": str(number)}, ok=False)
            assert crashed.returncode != 0
            log = h.journal(path)
            report = json.loads(h.run(path, "run", "--resume", str(log)).stdout)
            assert report["finished"] and report["luau_errors"] == 0, report
            assert len(archives(log)) == (0 if number < commit else 1)


def invalid_directive():
    path = h.workspace("history-invalid", INIT)
    override = path / ".harness/override/std"
    override.mkdir(parents=True)
    (override / "loop.luau").write_text('''local base=require("base:std/loop")
local M={init=base.init}
function M.step(s,e)
 local state,command=base.step(s,e)
 if command.archive then command.archive.cut=2 end
 return state,command
end
return M
''')
    with h.provider(script()) as (url, calls):
        result = start(path, url, ok=False)
        assert result.returncode and "history_archive_invalid" in result.stderr, result.stderr
        assert not archives(h.journal(path))


def scope_and_branch():
    path = h.workspace("history-scope", INIT)
    root = 0
    # Distinguish the child by its first user message, rather than parent tool code.
    def scoped(body, index):
        if h.task_text(body).startswith("HISTORY_CHILD"):
            if "CHILD_ISOLATED" not in h.body_text(body):
                return h.tool('assert(history==nil);print("CHILD_ISOLATED")')
            return "child done"
        nonlocal root
        root += 1
        if root == 1:
            return h.tool('assert(history().status=="unavailable");assert(agent("HISTORY_CHILD",{max_turns=3}).finished);print("ROOT_SCOPE_OK")')
        return "done"
    with h.provider(scoped) as (url, calls):
        report = json.loads(start(path, url).stdout)
        assert report["finished"] and report["luau_errors"] == 0, report
    parent = h.workspace("history-parent", INIT)
    with h.provider(script()) as (url, calls):
        start(parent, url)
    branch = h.workspace("history-branch", INIT)
    def branch_reply(body, index):
        return h.tool('assert(history().status=="unavailable");print("BRANCH_ISOLATED")') if index == 0 else "done"
    with h.provider(branch_reply) as (url, calls):
        report = json.loads(h.run(branch, "run", "--branch", str(h.journal(parent)),
            "--prompt-file", str(branch / "task.md"), "--model", "mock", "--base-url", url,
            "--key-env", "HX_TEST_KEY").stdout)
        assert report["finished"] and report["luau_errors"] == 0, report


def rpi_compaction():
    import rpi_checks as rpi
    path = rpi.workspace("history-compaction")
    (path / ".harness/init.luau").write_text(INIT)
    base = rpi.Script()
    research_calls = 0
    carried = False
    def answer(body, index):
        nonlocal research_calls, carried
        if is_summary(body):
            return "Research continues; old diagnostic text omitted."
        name = rpi.stage(body)
        if name == "research_questions":
            research_calls += 1
            if research_calls == 1:
                return h.tool(FIRST)
            if research_calls == 2:
                return {"code": 'print("tail")', "tokens": 23000}
            if research_calls == 3:
                return h.tool(GET)
        if name == "research" and not carried:
            carried = True
            return h.tool('local p=history({id=durable.get("old_entry")});assert(p.text:find("HISTORY_NEEDLE=8bd7e395",1,true));print("PRIOR_STAGE_HISTORY_OK")')
        return base(body, index)
    with h.provider(answer) as (url, calls):
        report = rpi.report(rpi.start(path, url, "--context-window", "40000", "--max-output", "1000"))
        assert report["finished"], report
        log = h.journal(path)
        saved = archives(log)
        assert len(saved) == 1 and saved[0]["archive"]["stage_id"].startswith("research_questions"), saved
        assert carried
        assert any(rec.get("tool") == "history" and rec.get("workflow_stage", "").startswith("research_questions") for rec in h.records(log))
        for rec in h.records(log):
            if rec["kind"] == "checkpoint":
                state = json.loads((log.parent / "blobs" / rec["blob"]).read_text())
                worker = state.get("controller", {}).get("worker", {})
                assert worker.get("report", {}).get("luau_errors", 0) == 0, worker


if __name__ == "__main__":
    commit = roundtrip()
    print("PASS compaction retrieval, repeated compaction and completed resume", flush=True)
    failed_summary()
    print("PASS empty summaries create no archive", flush=True)
    crash_boundaries(commit)
    print("PASS atomic archive recovery on both sides of commit", flush=True)
    invalid_directive()
    print("PASS invalid directive rejected before commit", flush=True)
    scope_and_branch()
    print("PASS child and branch isolation", flush=True)
    rpi_compaction()
    print("PASS RPI inspect-stage compaction and retrieval across stage retirement", flush=True)
