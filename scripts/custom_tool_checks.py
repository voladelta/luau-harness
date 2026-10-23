#!/usr/bin/env python3
"""Custom tool registration, transport, trust and recovery checks. No live API keys."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).resolve().parents[1]
HX = ROOT / "target/release/hx"
CALLS = []


def text(content):
    return "\n".join(part.get("text", "") for part in content) if isinstance(content, list) else content or ""


def model_reply(body):
    """SSoT requests answer from their seed; a run's agent runs MODEL_CODE once (if set), then says done."""
    system = body["instructions"]
    if "<random_string>" in system:
        seed = system.split("<random_string>")[1].split("</random_string>")[0]
        return [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "" if "cut off" in text(body["input"][0]["content"]) else f"<thinking>axes</thinking><answer> idea {seed} </answer>"}]}]
    if MODEL_CODE and not any(m.get("role") == "assistant" or m.get("type") == "function_call" for m in body["input"]):
        return [{"type": "function_call", "call_id": "c1", "name": "luau", "arguments": json.dumps({"code": MODEL_CODE[0]})}]
    return [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}]


MODEL_CODE = []


class API(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.reply()

    def do_POST(self):
        self.reply()

    def reply(self):
        url = urlsplit(self.path)
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"null")
        key = self.headers.get("X-API-Key")
        CALLS.append((self.command, url.path, parse_qs(url.query), body, key))
        if url.path == "/responses":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"output": model_reply(body), "usage": {"input_tokens": 10, "output_tokens": 5}}).encode())
            return
        status = 401 if url.path == "/unauthorized" else 302 if url.path == "/redirect" else 200
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        if status == 302:
            self.send_header("Location", "/leaked")
        self.end_headers()
        if url.path == "/invalid":
            self.wfile.write(b"not json")
            return
        self.wfile.write(json.dumps({"results": [{"title": "example", "text": "page"}],
                                    "errors": [{"url": "https://bad.test", "error": "timeout"}],
                                    "query": parse_qs(url.query), "body": body, "echo": key}).encode())


class Tools(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), API)
        cls.base = f"http://127.0.0.1:{cls.server.server_port}"
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.ws = self.root / "ws"
        self.dir = self.ws / ".harness/tools"
        self.dir.mkdir(parents=True)
        self.env = {k: v for k, v in os.environ.items() if k not in ("TINYFISH_API_KEY", "HX_TRUST")}
        self.env.update(HX_TRUST="1", XDG_CONFIG_HOME=str(self.root / "config"))
        CALLS.clear()

    def command(self, *args, env=None):
        result = subprocess.run([str(HX), *args, "--workspace", str(self.ws)],
                                env=self.env if env is None else env, text=True, capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        return result.stdout + result.stderr

    def run_code(self, *code, env=None):
        return self.command("exec", *code, env=env)

    def tool(self, name="probe", path="/search", read_only=True, secret=None):
        spec = {"url": self.base + path, "method": "GET", "header": "X-API-Key", "read_only": read_only}
        if secret:
            spec["secret"] = secret
        fields = ",".join(k + "=" + (str(v).lower() if isinstance(v, bool) else json.dumps(v)) for k, v in spec.items())
        source = 'return {http={get={' + fields + '}},tools={get={signature="get(args: table): table",description="mock get",run=function(args) return http("get",args) end}}}'
        (self.dir / f"{name}.lua").write_text(source)

    def test_discovery_isolation_and_readonly_namespace(self):
        source = 'local n=0; return {tools={go={signature="go(): number",description="counter",run=function() n+=1; return n end}}}'
        (self.dir / "counter.luau").write_text(source)
        out = self.run_code('print(counter.go(),counter.go(),vars().counter,http,require); print(pcall(function() counter.go=nil end))')
        self.assertIn("1\t1\tnil\tnil\tnil", out)
        self.assertIn("false", out)

    def test_missing_and_blank_credentials_reject_whole_namespace(self):
        self.tool(secret="TINYFISH_API_KEY")
        for key in (None, "", "   "):
            env = self.env.copy()
            if key is not None:
                env["TINYFISH_API_KEY"] = key
            out = self.run_code('print(probe)', env=env)
            self.assertIn("nil", out)
            self.assertIn("registration failed: missing credential TINYFISH_API_KEY", out)
        self.assertEqual(CALLS, [])

    def test_credential_precedence_and_redaction(self):
        self.tool(secret="TINYFISH_API_KEY")
        user = self.root / "config/hx/secrets.toml"
        user.parent.mkdir(parents=True)
        user.write_text('TINYFISH_API_KEY="user-test-key"\n')
        code = 'local r=probe.get{query="space & café",page=0}; print(r.echo,r.query.query[1])'
        self.assertIn("[redacted]\tspace & café", self.run_code(code))
        self.assertEqual(CALLS[-1][-1], "user-test-key")
        project = self.ws / ".harness/secrets.toml"
        project.write_text('TINYFISH_API_KEY="project-test-key"\n')
        self.run_code(code)
        self.assertEqual(CALLS[-1][-1], "project-test-key")
        env = dict(self.env, TINYFISH_API_KEY="environment-test-key")
        out = self.run_code(code, env=env)
        self.assertNotIn("environment-test-key", out)
        self.assertEqual(CALLS[-1][-1], "environment-test-key")
        for path in (self.ws / ".harness/log").rglob("*"):
            if path.is_file():
                content = path.read_bytes()
                for key in (b"environment-test-key", b"project-test-key", b"user-test-key"):
                    self.assertNotIn(key, content, str(path))
        out = self.run_code('print(pcall(read,".harness/secrets.toml")); print(#search("project-test-key"))')
        self.assertNotIn("project-test-key", out)
        self.assertIn("credential file", out)

    def test_tinyfish_search_and_fetch_wire_contract(self):
        source = (ROOT / "examples/tools/tinyfish.lua").read_text()
        source = source.replace("https://api.search.tinyfish.ai", self.base + "/search").replace("https://api.fetch.tinyfish.ai", self.base + "/fetch")
        (self.dir / "tinyfish.lua").write_text(source)
        env = dict(self.env, TINYFISH_API_KEY="tinyfish-test-key")
        out = self.run_code('local s=tinyfish.search{query="Luau & Rust",include_domains="github.com,luau.org",page=0}; print(s.results[1].title); local f=tinyfish.fetch{urls={"https://a.test","https://bad.test"},ttl=0,links=true,include_selectors={"main"}}; print(f.results[1].text,f.errors[1].error)', env=env)
        self.assertIn("example", out)
        self.assertIn("page\ttimeout", out)
        self.assertEqual(CALLS[0][:3], ("GET", "/search", {"query": ["Luau & Rust"], "include_domains": ["github.com,luau.org"], "page": ["0"]}))
        self.assertEqual(CALLS[1][0:2], ("POST", "/fetch"))
        self.assertEqual(CALLS[1][3], {"urls": ["https://a.test", "https://bad.test"], "ttl": 0, "links": True, "include_selectors": ["main"]})
        self.assertTrue(all(call[-1] == "tinyfish-test-key" for call in CALLS))
        count = len(CALLS)
        out = self.run_code('print(pcall(tinyfish.search,{query=""})); print(pcall(tinyfish.fetch,{urls={}})); print(pcall(tinyfish.fetch,{urls={"https://a.test","https://b.test"},if_none_match="v1"}))', env=env)
        self.assertEqual(sum(line.startswith("false") for line in out.splitlines()), 3)
        self.assertEqual(len(CALLS), count)

    def test_registration_is_pure_atomic_and_bounded(self):
        (self.dir / "bad.lua").write_text('write("bad.txt","bad"); return {}')
        (self.dir / "spin.lua").write_text('while true do end')
        (self.dir / "partial.lua").write_text('return {tools={a={signature="a()",description="ok",run=function() end},b={signature="b()",description="bad"}}}')
        self.tool()
        out = self.run_code('print(bad,spin,partial,type(probe.get))')
        self.assertIn("nil\tnil\tnil\tfunction", out)
        self.assertIn("time budget exceeded", out)
        self.assertFalse((self.ws / "bad.txt").exists())
        self.assertEqual(CALLS, [])

    def test_collisions_and_duplicates_reject_without_overwriting(self):
        self.tool("print")
        self.tool("tools")
        self.tool("duplicate")
        (self.dir / "duplicate.luau").write_text((self.dir / "duplicate.lua").read_text())
        out = self.run_code('print(type(print),type(tools),duplicate)')
        self.assertIn("function\ttable\tnil", out)
        self.assertIn("already in use", out)
        self.assertIn("reserved tool namespace", out)
        self.assertIn("duplicate tool namespace", out)

    def test_workspace_trust_covers_tools_and_symlinks(self):
        self.tool()
        env = self.env.copy()
        env.pop("HX_TRUST")
        self.assertIn("nil", self.run_code('print(probe)', env=env))
        self.command("trust", "--yes", env=env)
        self.assertIn("function", self.run_code('print(type(probe.get))', env=env))
        with (self.dir / "probe.lua").open("a") as file:
            file.write("\n-- edited\n")
        self.assertIn("nil", self.run_code('print(probe)', env=env))
        self.command("trust", "--yes", env=env)
        (self.dir / "link.lua").symlink_to(self.dir / "probe.lua")
        result = subprocess.run([str(HX), "trust", "--yes", "--workspace", str(self.ws)], env=env, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("symlinks", result.stderr)
        self.assertIn("tool must be a regular file", self.run_code('print(link)'))

    def test_effect_capabilities_and_endpoint_grants(self):
        (self.ws / "a.txt").write_text("hello")
        source = 'return {CAPS tools={get={signature="get(): string",description="read",run=function() return read("a.txt") end},bad={signature="bad()",description="write",run=function() write("bad.txt","bad") end}}}'
        (self.dir / "reader.lua").write_text(source.replace("CAPS", 'capabilities={fs={"read"}},'))
        self.assertIn("hello", self.run_code('print(reader.get())'))
        self.assertIn("capability_denied", self.run_code('print(pcall(reader.bad))'))
        self.assertFalse((self.ws / "bad.txt").exists())
        self.tool()
        self.assertIn("GET parameters must be strings, numbers or booleans", self.run_code('print(pcall(function() return probe.get{query={nested=true}} end))'))
        file = self.dir / "probe.lua"
        file.write_text(file.read_text().replace('http("get",args)', 'http("undeclared",args)'))
        self.assertIn("undeclared HTTP endpoint", self.run_code('print(pcall(probe.get,{}))'))
        self.assertEqual(CALLS, [])

    def test_http_errors_no_retries_or_redirects(self):
        env = dict(self.env, TINYFISH_API_KEY="fake-private-key")
        self.tool(path="/unauthorized", secret="TINYFISH_API_KEY")
        out = self.run_code('print(pcall(probe.get,{}))', env=env)
        self.assertIn("HTTP 401", out)
        self.assertNotIn("fake-private-key", out)
        self.assertEqual(len(CALLS), 1)
        self.tool(path="/redirect", secret="TINYFISH_API_KEY")
        self.assertIn("HTTP 302", self.run_code('print(pcall(probe.get,{}))', env=env))
        self.assertEqual([c[1] for c in CALLS], ["/unauthorized", "/redirect"])

    def test_external_endpoints_use_existing_approval_gate(self):
        self.tool(read_only=False)
        self.assertIn("approval_required", self.run_code('print(pcall(probe.get,{}))'))
        self.assertEqual(CALLS, [])
        (self.ws / ".harness/policy.toml").write_text('[approval]\nallow=["http probe.get"]\n')
        self.assertIn("example", self.run_code('print(probe.get{}.results[1].title)'))
        self.assertEqual(len(CALLS), 1)

    def test_prompt_lists_only_registered_methods(self):
        self.tool()
        self.tool("missing", secret="HX_MISSING_TEST_CREDENTIAL")
        self.env.pop("HX_MISSING_TEST_CREDENTIAL", None)
        task = self.root / "task.txt"
        task.write_text("Reply done.")
        self.command("run", "--prompt-file", str(task), "--model", "mock", "--base-url", self.base,
                     "--key-env", "HX_TEST_MODEL_KEY", "--max-turns", "2", env=dict(self.env, HX_TEST_MODEL_KEY="model-test-key"))
        self.assertGreater(len(CALLS), 0)
        system = CALLS[0][3]["instructions"]
        self.assertIn("probe.get(args: table): table", system)
        self.assertNotIn("missing.get", system)
        self.assertNotIn("model-test-key", system)

    def run_model(self, code, *flags):
        MODEL_CODE[:] = [code]
        self.addCleanup(MODEL_CODE.clear)
        task = self.root / "task.txt"
        task.write_text("Use the tool.")
        self.command("run", "--prompt-file", str(task), "--model", "mock", "--base-url", self.base,
                     "--key-env", "HX_TEST_MODEL_KEY", "--max-turns", "3", *flags, env=dict(self.env, HX_TEST_MODEL_KEY="model-test-key"))
        ssot = [c[3] for c in CALLS if "<random_string>" in c[3]["instructions"]]
        result = [m["output"] for c in CALLS for m in c[3]["input"] if m.get("type") == "function_call_output"]
        return ssot, result[-1] if result else ""

    def test_ssot_samples_from_the_runs_model_with_fresh_seeds(self):
        (self.dir / "ssot.lua").write_text((ROOT / "examples/tools/ssot.lua").read_text())
        ssot, out = self.run_model('local r = ssot.sample{prompt="p", n=3}; print(#r.answers, #r.errors); for _, a in r.answers do print(a.answer == "idea " .. a.seed) end', "--reasoning", "high")
        self.assertEqual(len(ssot), 3)
        self.assertTrue(all(b["model"] == "mock" and "reasoning" not in b and text(b["input"][0]["content"]) == "p" for b in ssot))
        seeds = {b["instructions"].split("<random_string>")[1].split("<")[0] for b in ssot}
        self.assertEqual(len(seeds), 3)
        self.assertTrue(all(len(seed) == 32 for seed in seeds))
        self.assertIn("3\t0", out)
        self.assertEqual(out.count("true"), 3)
        CALLS.clear()
        ssot, out = self.run_model('local r = ssot.sample{prompt="cut off", n=2, effort="low", max_tokens=100}; print(#r.answers, r.errors[1]); print(pcall(ssot.sample, {prompt="p", effort="huge"}))')
        self.assertEqual([(b["reasoning"], b["max_output_tokens"]) for b in ssot], [({"effort": "low"}, 100)] * 2)
        self.assertIn("effort must be none, low, medium, high, xhigh or max", out)
        self.assertIn("0\tno answer in reply", out)

    def test_llm_needs_capability_and_a_run(self):
        source = 'return {CAPS tools={go={signature="go(): table",description="chat",run=function() return llm({{system="s",prompt="p"}}) end}}}'
        (self.dir / "plain.lua").write_text(source.replace("CAPS", ""))
        (self.dir / "chatty.lua").write_text(source.replace("CAPS", 'capabilities={llm={"chat"}},'))
        out = self.run_code('print(pcall(plain.go)); print(pcall(chatty.go))')
        self.assertIn("capability_denied: tool:plain lacks llm.chat", out)
        self.assertIn("llm requires hx run", out)
        _, out = self.run_model('print(pcall(chatty.go)); print(pcall(llm, {}))')
        self.assertIn("true\t<table: 1 rows {ok,value}>", out)
        self.assertIn("false\tattempt to call a nil value", out)  # the model itself gets no llm

    def test_json_escaped_credential_is_redacted(self):
        self.tool(secret="TINYFISH_API_KEY")
        env = dict(self.env, TINYFISH_API_KEY='fake-"quoted"-key')
        out = self.run_code('print(probe.get{}.echo)', env=env)
        self.assertIn("[redacted]", out)
        self.assertNotIn("quoted", out)

    def test_unreadable_external_response_remains_uncertain(self):
        self.tool(path="/invalid", read_only=False)
        (self.ws / ".harness/policy.toml").write_text('[approval]\nallow=["http probe.get"]\n')
        out = self.run_code('print(pcall(probe.get,{})); print(pcall(write,"after.txt","bad"))')
        self.assertIn("unknown_outcome: HTTP response is not valid JSON", out)
        self.assertIn("unresolved mutations block new mutations", out)
        self.assertFalse((self.ws / "after.txt").exists())
        self.assertEqual(len(CALLS), 1)

    def test_frozen_source_survives_edit_during_session(self):
        source = 'return {tools={go={signature="go(): string",description="version",run=function() return "original" end}}}'
        file = self.dir / "version.lua"
        file.write_text(source)
        process = subprocess.Popen([str(HX), "exec", "--workspace", str(self.ws), 'print(version.go())', 'sh("sleep 0.2"); print(version.go())'], env=self.env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: process.poll() is None and process.kill())
        first = process.stdout.readline()
        self.assertIn("original", first)
        file.write_text(source.replace("original", "edited"))
        out, err = process.communicate(timeout=10)
        self.assertEqual(process.returncode, 0, err)
        self.assertIn("original", out)
        self.assertNotIn("edited", out)
        self.assertIn("edited", self.run_code('print(version.go())'))


if __name__ == "__main__":
    unittest.main(verbosity=2)
