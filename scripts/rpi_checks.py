#!/usr/bin/env python3
"""Offline RPI lifecycle checks with the real CLI and native stage.submit.

No provider credentials are used. Process outputs are kept outside the observed
workspace, so acceptance checks measure source changes rather than test logging.
"""
import contextlib
import json
import os
from pathlib import Path
import subprocess
import time
import redesign_checks as harness

BASE = harness.BASE / "rpi"
BASE.mkdir(exist_ok=True)
OUTPUTS = BASE / "outputs"
OUTPUTS.mkdir(exist_ok=True)
HEADINGS = {
    "# Research questions": "research_questions",
    "# Research\n": "research",
    "# System design": "system_design",
    "# Program design and implementation outline": "program_design",
    "# Implementation\n": "implementation",
    "# Verification and diff review": "verification",
    "# Repair\n": "repair",
}
passed = 0


def workspace(name):
    path = BASE / name
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(['git', 'init', '-q'], cwd=path, check=True)
    subprocess.run(['git', 'config', 'user.email', 'test@test.local'], cwd=path, check=True)
    subprocess.run(['git', 'config', 'user.name', 'test'], cwd=path, check=True)
    (path / ".harness").mkdir(exist_ok=True)
    (path / ".harness/init.luau").write_text('require("std/recheck").enabled=false\n')
    (path / "task.md").write_text("Create result.txt containing verified. Preserve all supplied owner feedback.")
    return path


def run(path, *args, env=None, ok=True):
    environment = {**harness.ENV, **(env or {})}
    environment = {key: value for key, value in environment.items() if value is not None}
    result = subprocess.run([str(harness.HX), *args, "--workspace", str(path)],
                            env=environment, capture_output=True, text=True, timeout=45)
    (OUTPUTS / f"{path.name}.stdout").write_text(result.stdout)
    (OUTPUTS / f"{path.name}.stderr").write_text(result.stderr)
    if ok:
        assert result.returncode == 0, (args, result.returncode, result.stdout, result.stderr)
    return result


def start(path, url, *flags, env=None, ok=True):
    return run(path, "run", "--rpi", "--prompt-file", str(path / "task.md"),
               "--model", "mock", "--base-url", url, "--key-env", "HX_TEST_KEY",
               "--max-turns", "50", "--yes", *flags, env=env, ok=ok)


def report(result):
    return json.loads(result.stdout)


def gate(result):
    # The saved host frame is authoritative regardless of report presentation.
    log = Path(result["log"])
    records = harness.records(log)
    checkpoint = next(r for r in reversed(records) if r["kind"] == "checkpoint" and r.get("lane") == "root")
    state = harness.stored(log, checkpoint["blob"])
    return state["gate"]


def stage(body):
    text = harness.body_text(body)
    candidates = [(text.find(heading.replace("\n", "\\n")), name) for heading, name in HEADINGS.items()]
    candidates = [(i, name) for i, name in candidates if i >= 0]
    assert candidates, text[:500]
    return min(candidates)[1]


def submit(name, extra="", outcome="complete", questions="{}", evidence="{}"):
    return ('stage.submit({outcome=' + json.dumps(outcome) + ',document=' + json.dumps("# " + name + "\nEvidence and decisions recorded.")
            + ',summary=' + json.dumps(name + " complete") + ',questions=' + questions
            + ',evidence=' + evidence + ',' + extra + '})')


class Script:
    def __init__(self, *, needs_input=False, repair=False, guards=False, candidate=False):
        self.seen = []
        self.counts = {}
        self.needs_input = needs_input
        self.repair = repair
        self.guards = guards
        self.candidate = candidate

    def __call__(self, body, index):
        name = stage(body)
        self.seen.append(name)
        self.counts[name] = self.counts.get(name, 0) + 1
        n = self.counts[name]
        if self.candidate and name == "research_questions" and n == 1:
            return "Premature prose completion"
        if self.needs_input and name == "research" and n == 1:
            return harness.tool(submit(name, outcome="needs_input", questions='{"Which greeting?"}'))
        if name == "program_design":
            return harness.tool(submit(name, 'checks={{id="content",command="test $(cat result.txt) = verified",cwd=".",timeout=60,requirements={"content"}}},'))
        if name == "implementation":
            content = "wrong" if self.repair else "verified"
            return harness.tool('write("result.txt",' + json.dumps(content) + '); ' + submit(name))
        if name == "repair":
            return harness.tool('write("result.txt","verified"); ' + submit(name))
        if name == "verification":
            return harness.tool('local r=sh("test $(cat result.txt) = verified",{cwd=".",timeout=60}); '
                                + submit(name, 'findings={},', evidence='{{check_id="content",operation_id=r.operation_id}}'))
        prefix = ""
        if self.guards and name == "research_questions":
            prefix = 'assert(not pcall(write,"escape.txt","bad")); assert(not pcall(sh,"true")); '
        if self.guards and name == "research":
            prefix = 'assert(leaked_global==nil); '
        if self.guards and name == "research_questions":
            prefix += 'leaked_global=41; '
            prefix += 'local rows=search("Create result.txt",{glob="**/*",literal=true,max=20}); assert(#rows>0); for _,row in rows do assert(not row.file:find(".agents/artifacts",1,true)) end; assert(#search("Create result.txt",{path=".agents/artifacts",glob="**/*",literal=true,max=10})>0); '

        return harness.tool(prefix + submit(name))


def auto_success():
    path = workspace("auto")
    script = Script(guards=True, candidate=True)
    with harness.provider(script) as (url, calls):
        value = report(start(path, url))
        assert value["finished"] and value["status"] == "completed", value
        assert (path / "result.txt").read_text() == "verified"
        assert not (path / "escape.txt").exists()
        assert set(script.seen) == set(HEADINGS.values()) - {"repair"}, script.seen
        count = len(calls)
        again = report(run(path, "run", "--resume", value["log"]))
        assert again["finished"] and len(calls) == count
        refused = run(path, "run", "--resume", value["log"], "--rpi", ok=False)
        assert refused.returncode and len(calls) == count


def manual_reviews():
    path = workspace("manual")
    script = Script()
    with harness.provider(script) as (url, calls):
        value = report(start(path, url, "--manual"))
        gates = []
        # Resume must use frozen module bytes even if local config becomes invalid.
        (path / ".harness/init.luau").write_text('error("changed config must not execute on resume")')
        revised = False
        while not value["finished"]:
            request = gate(value)
            gates.append(request["kind"])
            count = len(calls)
            paused = report(run(path, "run", "--resume", value["log"]))
            assert not paused["finished"] and len(calls) == count
            wrong = run(path, "review", value["log"], "--gate", "stale-gate", "--decision", "approve", ok=False)
            assert wrong.returncode and len(calls) == count
            if not revised:
                feedback = OUTPUTS / "feedback.md"
                feedback.write_text("OWNER_FEEDBACK_SENTINEL: keep scope minimal.")
                run(path, "review", value["log"], "--gate", request["gate_id"], "--decision", "revise", "--input-file", str(feedback))
                revised = True
            else:
                run(path, "review", value["log"], "--gate", request["gate_id"], "--decision", "approve")
            before_resume = len(calls)
            value = report(run(path, "run", "--resume", value["log"]))
            if request["kind"] == "acceptance":
                assert len(calls) == before_resume, "final owner approval dispatched a model call"
            assert len(gates) <= 7, gates
        assert gates.count("review") == 5 and gates[-1] == "acceptance", gates
        assert "OWNER_FEEDBACK_SENTINEL" in harness.body_text(calls[-1])


def input_and_repair():
    path = workspace("input-repair")
    script = Script(needs_input=True, repair=True)
    with harness.provider(script) as (url, calls):
        value = report(start(path, url))
        request = gate(value)
        assert request["kind"] == "input", value
        answer = OUTPUTS / "answer.md"
        answer.write_text("OWNER_ANSWER_SENTINEL: use verified.")
        run(path, "review", value["log"], "--gate", request["gate_id"], "--decision", "answer", "--input-file", str(answer))
        value = report(run(path, "run", "--resume", value["log"]))
        assert value["finished"] and "repair" in script.seen and script.counts["verification"] == 2, (value, script.seen)
        assert "OWNER_ANSWER_SENTINEL" in harness.body_text(calls[-1])
        # The restarted research worker sees the question it asked, not only the answer.
        restarted = [harness.body_text(body) for body in calls if stage(body) == "research"][-1]
        assert "restart-cause" in restarted and "Which greeting?" in restarted
        # Repair sees the rejected verification it repairs.
        repair = next(harness.body_text(body) for body in calls if stage(body) == "repair")
        assert "REJECTED" in repair


def acceptance_drift():
    path = workspace("drift")
    with harness.provider(Script()) as (url, calls):
        value = report(start(path, url, "--manual"))
        for _ in range(4):
            request = gate(value)
            run(path, "review", value["log"], "--gate", request["gate_id"], "--decision", "approve")
            value = report(run(path, "run", "--resume", value["log"]))
        request = gate(value)
        assert request["kind"] == "acceptance"
        (path / "result.txt").write_text("changed after verification")
        count = len(calls)
        result = run(path, "review", value["log"], "--gate", request["gate_id"], "--decision", "approve", ok=False)
        assert result.returncode and len(calls) == count


def native_submission_guards():
    for mode in ("double", "after", "bad"):
        path = workspace("guard-" + mode)
        count = 0
        def answer(body, index):
            nonlocal count
            if stage(body) == "research_questions" and count == 0:
                count += 1
                if mode == "double":
                    return harness.tool(submit("research_questions") + '; ' + submit("forged"))
                if mode == "after":
                    return harness.tool(submit("research_questions") + '; write("escape.txt","bad")')
                return harness.tool('stage.submit({outcome="complete",document=4})')
            return script(body, index)
        script = Script()
        with harness.provider(answer) as (url, calls):
            value = report(start(path, url))
            assert value["finished"] and not (path / "escape.txt").exists(), value
            retries = [body for body in calls if stage(body) == "research_questions"]
            assert len(retries) >= 2, "invalid submission chunk advanced without correction"
            assert any(m.get("role") == "tool" or m.get("type") == "function_call_output" for m in harness.body_messages(retries[-1]))


def crash_delivery():
    control = workspace("crash-control")
    with harness.provider(Script()) as (url, _):
        value = report(start(control, url))
        checkpoints = [r for r in harness.records(Path(value["log"])) if r["kind"] == "checkpoint" and r.get("lane") == "root"]
        # Crash both immediately before and at an early durable stage handoff.
        selected = []
        for r in checkpoints:
            state = harness.stored(value["log"], r["blob"])
            frame = state.get("workflow") or {}
            if frame.get("pending_event") and frame.get("artifacts"):
                selected = [r["seq"] - 1, r["seq"]]
                break
        assert selected, checkpoints
    for sequence in selected:
        path = workspace(f"crash-{sequence}")
        with harness.provider(Script()) as (url, calls):
            result = start(path, url, env={"HX_TEST_CRASH_AFTER_RECORD": str(sequence)}, ok=False)
            assert result.returncode != 0
            log = harness.journal(path)
            value = report(run(path, "run", "--resume", str(log)))
            assert value["finished"], value
            count = len(calls)
            assert report(run(path, "run", "--resume", str(log)))["finished"] and len(calls) == count


def opt_in_and_preflight():
    path = workspace("normal")
    with harness.provider(lambda body, i: harness.tool('assert(stage==nil);print("normal")') if i == 0 else "normal done") as (url, calls):
        value = report(run(path, "run", "--prompt-file", str(path / "task.md"), "--model", "mock", "--base-url", url, "--key-env", "HX_TEST_KEY"))
        assert value["finished"] and "workflow" not in value and len(calls) == 2, value
    for index, flags in enumerate((("--manual",), ("--pack", "std/rpi"), ("--rpi", "--rpi"))):
        path = workspace("preflight-" + str(index))
        value = run(path, "run", *flags, ok=False)
        assert value.returncode == 2 and not harness.logs(path).exists(), value.stderr
    non_git = BASE / "non-git"
    non_git.mkdir(parents=True, exist_ok=True)
    (non_git / "task.md").write_text("task")
    res = run(non_git, "run", "--rpi", "--prompt-file", str(non_git / "task.md"), "--model", "mock", ok=False)
    assert res.returncode != 0 and "requires a Git repository" in res.stderr, res.stderr


def saved_state(log):
    log = Path(log)
    record = next(r for r in reversed(harness.records(log)) if r["kind"] == "checkpoint" and r.get("lane") == "root")
    return harness.stored(log, record["blob"])


def deadline(log):
    log = Path(log)
    manifest = next(r for r in harness.records(log) if r["kind"] == "manifest")
    return json.loads((log.parent / "blobs" / manifest["blob"]).read_text())["deadline"]


def expire(log):
    time.sleep(max(0, deadline(log) - time.time()) + 0.05)


def post_submit_batch():
    path = workspace("post-submit-batch")
    script = Script()
    attempted = False
    def answer(body, index):
        nonlocal attempted
        if "FORBIDDEN_BATCH_CHILD" in harness.task_text(body):
            raise AssertionError("post-submit batch reached a child provider")
        if stage(body) == "research_questions" and not attempted:
            attempted = True
            return harness.tool(submit("research_questions") + '; batch({{op="agent",args={"FORBIDDEN_BATCH_CHILD"}}})')
        return script(body, index)
    with harness.provider(answer) as (url, calls):
        value = report(start(path, url))
        assert value["finished"], value
        assert all("FORBIDDEN_BATCH_CHILD" not in harness.task_text(body) for body in calls)
        records = harness.records(Path(value["log"]))
        assert not any(r.get("kind") == "model_intent" and r.get("depth", 0) > 0 for r in records)
        assert len([body for body in calls if stage(body) == "research_questions"]) >= 2


@contextlib.contextmanager
def manual_at(name, kind, wall):
    """A manual run approved up to its first `kind` gate, with the wall budget doubled until one is reached.
    Parallel suites can spend a short budget before the gate; only behavior after expiry is under test."""
    for attempt in range(4):
        path = workspace(f"{name}-{attempt}")
        with harness.provider(Script()) as (url, calls):
            value = report(start(path, url, "--manual", "--max-wall", str(wall)))
            for _ in range(8):
                request = gate(value)
                if not request or request["kind"] == kind or time.time() >= deadline(value["log"]):
                    break
                run(path, "review", value["log"], "--gate", request["gate_id"], "--decision", "approve")
                value = report(run(path, "run", "--resume", value["log"]))
            if request and request["kind"] == kind:
                yield path, value, request, calls
                return
        assert time.time() >= deadline(value["log"]), (kind, request, value)
        wall *= 2
    raise AssertionError(f"{name}: no {kind} gate within a {wall / 2:g}s wall budget")


def expired_review_and_acceptance():
    # Intermediate expired gates stay inspectable without credentials or model work.
    with manual_at("expired-review", "review", 1) as (path, value, request, calls):
        expire(value["log"])
        count = len(calls)
        before = len([r for r in harness.records(Path(value["log"])) if r["kind"] == "model_intent"])
        for _ in range(2):
            paused = report(run(path, "run", "--resume", value["log"], env={"HX_TEST_KEY": None}))
            assert not paused["finished"] and gate(paused)["gate_id"] == request["gate_id"]
        assert len(calls) == count
        assert len([r for r in harness.records(Path(value["log"])) if r["kind"] == "model_intent"]) == before
    # Completing final acceptance after expiry is an administrative transition.
    with manual_at("expired-acceptance", "acceptance", 5) as (path, value, request, calls):
        expire(value["log"])
        count = len(calls)
        assert not report(run(path, "run", "--resume", value["log"], env={"HX_TEST_KEY": None}))["finished"]
        run(path, "review", value["log"], "--gate", request["gate_id"], "--decision", "approve", env={"HX_TEST_KEY": None})
        for _ in range(2):
            completed = report(run(path, "run", "--resume", value["log"], env={"HX_TEST_KEY": None}))
            assert completed["finished"], completed
        assert len(calls) == count


def missing_credentials_no_reservation():
    path = workspace("missing-key-review")
    with harness.provider(Script()) as (url, calls):
        value = report(start(path, url, "--manual"))
        request = gate(value)
        run(path, "review", value["log"], "--gate", request["gate_id"], "--decision", "approve")
        before = [r for r in harness.records(Path(value["log"])) if r["kind"] == "model_intent"]
        count = len(calls)
        run(path, "run", "--resume", value["log"], env={"HX_TEST_KEY": None}, ok=False)
        after = [r for r in harness.records(Path(value["log"])) if r["kind"] == "model_intent"]
        assert before == after and len(calls) == count
        verified = run(path, "log", "verify", value["log"], ok=False)
        assert "UNKNOWN MODEL" not in verified.stdout, verified.stdout


def decision_crash_exactly_once():
    for offset in (0, 1, 2):
        path = workspace("decision-crash-" + str(offset))
        with harness.provider(Script()) as (url, calls):
            value = report(start(path, url, "--manual"))
            request = gate(value)
            next_sequence = harness.records(Path(value["log"]))[-1]["seq"] + 1
            if offset == 0:
                crashed = run(path, "review", value["log"], "--gate", request["gate_id"], "--decision", "approve",
                              env={"HX_TEST_CRASH_AFTER_RECORD": str(next_sequence)}, ok=False)
            else:
                run(path, "review", value["log"], "--gate", request["gate_id"], "--decision", "approve")
                crashed = run(path, "run", "--resume", value["log"],
                              env={"HX_TEST_CRASH_AFTER_RECORD": str(next_sequence + offset)}, ok=False)
            assert crashed.returncode != 0, (offset, crashed.stdout, crashed.stderr)
            resumed = report(run(path, "run", "--resume", value["log"]))
            next_gate = gate(resumed)
            assert next_gate["gate_id"] != request["gate_id"], (offset, resumed)
            count = len(calls)
            for _ in range(2):
                assert gate(report(run(path, "run", "--resume", value["log"])))["gate_id"] == next_gate["gate_id"]
            state = saved_state(value["log"])["workflow"]
            decisions = [r for r in harness.records(Path(value["log"])) if r["kind"] == "owner_decision"]
            assert len(decisions) == 1 and state["consumed_decisions"].count(str(decisions[0]["seq"])) == 1
            assert len([item for item in state["owner_inputs"] if item["gate_id"] == request["gate_id"]]) == 1
            assert len(calls) == count


def post_check_source_change():
    path = workspace("post-check-edit")
    script = Script()
    changed = False
    def answer(body, index):
        nonlocal changed
        if stage(body) == "verification" and not changed:
            changed = True
            return harness.tool('local r=sh("test $(cat result.txt) = verified",{cwd=".",timeout=60}); write("result.txt","changed after check"); '
                                + submit("verification", 'findings={},', evidence='{{check_id="content",operation_id=r.operation_id}}'))
        return script(body, index)
    with harness.provider(answer) as (url, _):
        value = report(start(path, url))
        assert value["finished"] and "repair" in script.seen, value
        assert (path / "result.txt").read_text() == "verified"
        artifacts = value["artifact_refs"]
        verification = [a for a in artifacts if a["submission"].get("verification")]
        assert len(verification) == 2 and not verification[0]["submission"]["verification"]["passed"]
        assert verification[-1]["submission"]["verification"]["passed"]


def native_admission_ceiling():
    path = workspace("native-admission-ceiling")
    child_calls = 0
    def answer(body, index):
        nonlocal child_calls
        if harness.task_text(body).startswith("ADMISSION_CHILD"):
            child_calls += 1
            # Its requested budget is intentionally larger than the entire run.
            return harness.tool('print("child continues")') if child_calls < 6 else "child done"
        return harness.tool('local child=agent("ADMISSION_CHILD keep investigating",{max_turns=20}); print(child.finished)')
    with harness.provider(answer) as (url, calls):
        value = report(run(path, "run", "--rpi", "--prompt-file", str(path / "task.md"),
                           "--model", "mock", "--base-url", url, "--key-env", "HX_TEST_KEY",
                           "--max-turns", "4", "--yes"))
        assert not value["finished"] and "max_turns" in value.get("stopped", ""), value
        intents = [r for r in harness.records(Path(value["log"])) if r["kind"] == "model_intent"]
        assert child_calls > 0 and any(r.get("depth", 0) > 0 for r in intents), intents
        assert len(calls) == len(intents) == 4, (len(calls), intents)
        count = len(calls)
        for _ in range(2):
            resumed = report(run(path, "run", "--resume", value["log"]))
            assert not resumed["finished"] and "max_turns" in resumed.get("stopped", ""), resumed
        assert len(calls) == count
        assert len([r for r in harness.records(Path(value["log"])) if r["kind"] == "model_intent"]) == 4


def submission_protocol_errors():
    # Read errors that merely mention a submission are feedback; a workspace stage's check plan is a correctable error.
    path = workspace("submission-protocol")
    script = Script()
    tried = {"research_questions": 0, "implementation": 0}
    def answer(body, index):
        name = stage(body)
        if name == "research_questions" and tried[name] < 3:
            tried[name] += 1
            return harness.tool('read("src/submission_form.py")')
        if name == "implementation" and not tried[name]:
            tried[name] += 1
            return harness.tool('write("result.txt","verified"); ' + submit(name, 'checks={{id="extra",command="true"}},'))
        return script(body, index)
    with harness.provider(answer) as (url, calls):
        value = report(start(path, url))
        assert value["finished"], value
        implementation = [body for body in calls if stage(body) == "implementation"]
        assert len(implementation) == 2 and "cannot replace the accepted check plan" in harness.body_text(implementation[1])


def project_inventory_boundaries():
    # An entry verification cannot observe is refused at launch, by name, before any provider work.
    path = workspace("inventory-escape")
    os.symlink("/etc/hostname", path / "outside-link")
    subprocess.run(["git", "add", "outside-link"], cwd=path, check=True)
    refused = start(path, "http://127.0.0.1:9", ok=False)
    assert refused.returncode and "cannot observe the project inventory" in refused.stderr and "outside-link" in refused.stderr, refused.stderr
    assert not any(r["kind"] == "manifest" for log in (path / ".harness").glob("log/*.jsonl") for r in harness.records(log))
    # A nested repository is recorded as unobserved. An escape created mid-run fails verification with the reason
    # (instead of the process), and repair can fix it.
    path = workspace("inventory-nested")
    nested = path / "vendor/lib"
    nested.mkdir(parents=True)
    subprocess.run(["git", "init", "-q"], cwd=nested, check=True)
    (nested / "code.txt").write_text("nested")
    script = Script()
    def answer(body, index):
        name = stage(body)
        if name == "implementation":
            return harness.tool('write("result.txt","verified"); sh("ln -s /etc/hostname escape-link"); ' + submit(name))
        if name == "repair":
            return harness.tool('sh("rm escape-link"); ' + submit(name))
        return script(body, index)
    with harness.provider(answer) as (url, calls):
        result = start(path, url)
        assert "vendor/lib" in result.stderr, result.stderr
        value = report(result)
        assert value["finished"] and value["repairs"] == 1, value
        verification = [a["submission"]["verification"] for a in value["artifact_refs"] if a["submission"].get("verification")]
        assert "escape-link" in json.dumps(verification[0]["findings"]) and not verification[0]["passed"], verification[0]
        assert verification[-1]["passed"] and verification[-1]["unobserved"] == ["vendor/lib"], verification[-1]
        # Effect records, checkpoints and prompts carry the observation by reference; its blob holds the inventory once.
        log = Path(value["log"])
        observations = [r["project_observation"] for r in harness.records(log) if "project_observation" in r]
        assert observations and all(set(o) <= {"digest", "blob", "extra_inputs", "unobserved"} for o in observations), observations[0]
        inventory = json.loads((log.parent / "blobs" / observations[-1]["blob"]).read_text())["inventory"]
        assert inventory["vendor/lib"] == "unobserved:repository" and "result.txt" in inventory
        verify_bodies = [harness.body_text(body) for body in calls if stage(body) == "verification"]
        assert verify_bodies and not any("file:0:" in text for text in verify_bodies)


def check(name, fn):
    global passed
    if os.environ.get("HX_CHECK_FILTER") and os.environ["HX_CHECK_FILTER"] not in name:
        return
    fn()
    passed += 1
    print("PASS " + name, flush=True)


if __name__ == "__main__":
    check("opt-in guards and non-RPI compatibility", opt_in_and_preflight)
    check("full automatic route, inspect authority, fresh VMs and native submission", auto_success)
    check("manual revision, exact gates, feedback persistence and final administrative approval", manual_reviews)
    check("automatic necessary input and failed-check repair with fresh receipts", input_and_repair)
    check("final acceptance rejects changed source without provider work", acceptance_drift)
    check("malformed, double and post-submit chunks cannot advance", native_submission_guards)
    check("boundary crash delivery and repeated terminal resume", crash_delivery)
    check("post-submit batch cannot dispatch child provider calls", post_submit_batch)
    check("expired pauses and final approval require no provider or credentials", expired_review_and_acceptance)
    check("missing credentials after review create no model reservation", missing_credentials_no_reservation)
    check("owner decisions survive record and consumption crashes exactly once", decision_crash_exactly_once)
    check("source edits after passing checks force repair and fresh verification", post_check_source_change)
    check("native RPI admission ceiling counts descendant model calls across resume", native_admission_ceiling)
    check("only stage.submit refusals are protocol failures; workspace check plans are correctable", submission_protocol_errors)
    check("project inventory: launch refusal, unobserved nested repositories, non-fatal verification", project_inventory_boundaries)
    print(f"{passed} passed; artifacts: {BASE}")
