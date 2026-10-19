#!/usr/bin/env python3
"""OpenRouter tool registration, wire contract, bearer auth, media decoding, and 24h caching tests."""
import base64
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import unittest
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).resolve().parents[1]
HX = ROOT / "target/debug/hx"
CALLS = []


class OpenRouterMockAPI(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.reply()

    def do_POST(self):
        self.reply()

    def reply(self):
        url = urlsplit(self.path)
        content_len = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(content_len) or b"null") if content_len > 0 else None
        auth = self.headers.get("Authorization")
        CALLS.append((self.command, url.path, parse_qs(url.query), body, auth))

        if url.path == "/api/v1/credits":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"data": {"total_credits": 100.0, "total_usage": 25.5}}).encode())
            return

        if url.path == "/api/v1/models":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            models_data = [
                {
                    "id": "qwen/qwen-image-2.1-turbo",
                    "name": "Qwen Image",
                    "description": "C:\\new\\table\\read \n\r\t\b\f\x00 \"quoted\" 😀",
                    "created": 1000,
                    "context_length": 32000,
                    "architecture": {"output_modalities": ["image"]},
                    "pricing": {"prompt": "0.002", "completion": "0.002"},
                    "supported_parameters": ["seed", "aspect_ratio"]
                },
                {
                    "id": "google/veo-3.1",
                    "name": "Veo Video",
                    "created": 2000,
                    "context_length": 16000,
                    "architecture": {"output_modalities": ["video"]},
                    "pricing": {"prompt": "0.05", "completion": "0.05"},
                    "supported_parameters": ["duration", "resolution"]
                },
                {
                    "id": "elevenlabs/eleven-v4",
                    "name": "Eleven TTS",
                    "created": 1500,
                    "context_length": 8000,
                    "architecture": {"output_modalities": ["speech"]},
                    "pricing": {"prompt": "0.01", "completion": "0.01"},
                    "supported_parameters": ["voice", "speed"]
                },
                {
                    "id": "deepseek/deepseek-chat-v3",
                    "name": "DeepSeek Chat",
                    "created": 3000,
                    "context_length": 128000,
                    "architecture": {"output_modalities": ["text"]},
                    "pricing": {"prompt": "0.0001", "completion": "0.0002"},
                    "supported_parameters": ["tools", "temperature", "max_tokens"]
                },
            ]
            self.wfile.write(json.dumps({"data": models_data, "total_count": len(models_data)}).encode())
            return

        if url.path == "/qwen/qwen-image-2.1-turbo/llms.txt":
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.end_headers()
            self.wfile.write(b"# Qwen Image Guide\nDetailed guide for calling Qwen Image API.")
            return

        if url.path == "/docs/llms.txt":
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.end_headers()
            self.wfile.write(b"# Documentation Index\nDiscover all available pages.")
            return

        if url.path == "/api/v1/chat/completions":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({
                "id": "gen-chat-123",
                "model": body.get("model") if body else "mock-model",
                "choices": [{"message": {"role": "assistant", "content": "Mock text response"}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            }).encode())
            return

        if url.path == "/api/v1/images":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            # Valid PNG header (8 bytes) + chunk
            raw_png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01"
            b64_png = base64.b64encode(raw_png).decode("ascii")
            self.wfile.write(json.dumps({
                "created": 1234567890,
                "data": [{"b64_json": b64_png, "media_type": "image/png"}],
                "usage": {"cost": 0.02}
            }).encode())
            return

        if url.path == "/api/v1/audio/speech":
            self.send_response(200)
            self.send_header("Content-Type", "audio/mpeg")
            self.end_headers()
            raw_mp3 = b"\xff\xfb\x90d\x00\x00\x00\x00mock-audio-frame"
            self.wfile.write(raw_mp3)
            return

        if url.path == "/api/v1/videos":
            self.send_response(202)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({
                "id": "job-video-999",
                "generation_id": "gen-vid-999",
                "status": "pending",
                "polling_url": "/api/v1/videos/job-video-999"
            }).encode())
            return

        if url.path == "/api/v1/videos/job-video-999":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({
                "id": "job-video-999",
                "status": "completed",
                "unsigned_urls": ["https://storage.openrouter.ai/video999.mp4"],
                "usage": {"cost": 0.5}
            }).encode())
            return

        self.send_response(404)
        self.end_headers()


class OpenRouterToolTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), OpenRouterMockAPI)
        cls.port = cls.server.server_port
        cls.base = f"http://127.0.0.1:{cls.port}"
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
        self.env = {k: v for k, v in os.environ.items() if k not in ("OPENROUTER_API_KEY", "HX_TRUST")}
        self.env.update(HX_TRUST="1", XDG_CONFIG_HOME=str(self.root / "config"))
        CALLS.clear()

        # Install openrouter.lua with mock base URLs
        source = (ROOT / "examples/tools/openrouter.lua").read_text()
        source = source.replace("https://openrouter.ai", self.base)
        (self.dir / "openrouter.lua").write_text(source)

    def run_code(self, code, env=None, yolo=False):
        cmd = [str(HX), "exec"]
        if yolo:
            cmd.append("--yolo")
        cmd.extend(["--workspace", str(self.ws), code])
        result = subprocess.run(cmd, env=self.env if env is None else env, text=True, capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        return result.stdout + result.stderr

    def test_refuse_to_register_without_openrouter_api_key(self):
        """Tool registration must fail and openrouter remain nil if OPENROUTER_API_KEY is not in env."""
        env_no_key = dict(self.env)
        env_no_key.pop("OPENROUTER_API_KEY", None)
        out = self.run_code("print(vars().openrouter)", env=env_no_key)
        self.assertIn("registration failed: missing credential OPENROUTER_API_KEY", out)
        self.assertIn("nil", out)

        # Blank key also fails
        env_blank = dict(self.env, OPENROUTER_API_KEY="   ")
        out2 = self.run_code("print(vars().openrouter)", env=env_blank)
        self.assertIn("registration failed: missing credential OPENROUTER_API_KEY", out2)

    def test_credits_query_and_bearer_auth(self):
        """User can check remaining credits with explicit Bearer auth header."""
        env = dict(self.env, OPENROUTER_API_KEY="sk-or-test-secret-key")
        out = self.run_code(
            'local c = openrouter.credits(); print(c.total_credits, c.total_usage, c.remaining_credits)',
            env=env
        )
        self.assertIn("100\t25.5\t74.5", out)
        self.assertEqual(len(CALLS), 1)
        cmd, path, query, body, auth = CALLS[0]
        self.assertEqual((cmd, path), ("GET", "/api/v1/credits"))
        self.assertEqual(auth, "Bearer sk-or-test-secret-key")

    def test_bearer_opt_in_preserves_raw_authorization_headers(self):
        """Endpoints with bearer=false (or omitted) preserve custom Authorization headers (e.g. Basic ...)."""
        custom_tool = (
            'return {http={raw_auth={url="' + self.base + '/api/v1/credits",method="GET",secret="CUSTOM_AUTH_KEY",read_only=true}},'
            'tools={check={signature="check(): table",description="test",run=function() return http("raw_auth",{}) end}}}'
        )
        (self.dir / "custom_auth.lua").write_text(custom_tool)
        env = dict(self.env, CUSTOM_AUTH_KEY="Basic dXNlcjpwYXNz")
        CALLS.clear()
        out = self.run_code('print(type(custom_auth.check()))', env=env)
        self.assertIn("table", out)
        self.assertEqual(len(CALLS), 1)
        # Must be passed verbatim as Basic dXNlcjpwYXNz without prepending Bearer
        self.assertEqual(CALLS[0][4], "Basic dXNlcjpwYXNz")

    def test_models_fetch_and_24h_caching(self):
        """Models are fetched and cached for 24 hours unless force=true."""
        env = dict(self.env, OPENROUTER_API_KEY="sk-or-test-key")
        # First call: hits network, cached=false
        out1 = self.run_code(
            'local m = openrouter.models{output_modalities = "image"}; print(m.total_count, m.cached, m.data[1].id)',
            env=env
        )
        self.assertIn("1\tfalse\tqwen/qwen-image-2.1-turbo", out1)
        self.assertEqual(len(CALLS), 1)

        # Second call: hits cache, cached=true, NO additional HTTP request
        out2 = self.run_code(
            'local m = openrouter.models{output_modalities = "video"}; print(m.total_count, m.cached, m.data[1].id)',
            env=env
        )
        self.assertIn("1\ttrue\tgoogle/veo-3.1", out2)
        self.assertEqual(len(CALLS), 1)

        # Third call with force=true: bypasses cache and hits network
        out3 = self.run_code(
            'local m = openrouter.models{search = "chat", force = true}; print(m.total_count, m.cached, m.data[1].id)',
            env=env
        )
        self.assertIn("1\tfalse\tdeepseek/deepseek-chat-v3", out3)
        self.assertEqual(len(CALLS), 2)

    def test_models_supported_parameters_and_sort(self):
        """Models filtering by supported_parameters ('tools') and sorting ('pricing-low-to-high', 'context-high-to-low')."""
        env = dict(self.env, OPENROUTER_API_KEY="sk-or-test-key")
        # Tool filtering: only deepseek-chat supports "tools"
        out_tools = self.run_code(
            'local m = openrouter.models{supported_parameters = "tools"}; print(m.total_count, m.data[1].id)',
            env=env
        )
        self.assertIn("1\tdeepseek/deepseek-chat-v3", out_tools)

        # Sort pricing low to high: deepseek (0.0003) < qwen (0.004) < elevenlabs (0.02) < veo (0.1)
        out_price = self.run_code(
            'local m = openrouter.models{sort = "pricing-low-to-high"}; print(m.data[1].id, m.data[2].id)',
            env=env
        )
        self.assertIn("deepseek/deepseek-chat-v3\tqwen/qwen-image-2.1-turbo", out_price)

        # Sort context high to low: deepseek (128000) > qwen (32000) > veo (16000) > elevenlabs (8000)
        out_ctx = self.run_code(
            'local m = openrouter.models{sort = "context-high-to-low"}; print(m.data[1].id, m.data[2].id, m.data[3].id, m.data[4].id)',
            env=env
        )
        self.assertIn("deepseek/deepseek-chat-v3\tqwen/qwen-image-2.1-turbo\tgoogle/veo-3.1\televenlabs/eleven-v4", out_ctx)

    def test_catalog_cache_round_trip(self):
        env = dict(self.env, OPENROUTER_API_KEY="sk-or-test-key")
        code = 'local m = openrouter.models{}; print(string.byte(m.data[1].description, 1, #m.data[1].description))'
        fresh = self.run_code(code, env=env)
        cached = self.run_code(code, env=env)
        self.assertEqual(fresh, cached)
        self.assertEqual(len(CALLS), 1)
        raw = (self.ws / ".openrouter/models.json").read_text().split("\n", 1)[1]
        catalog = json.loads(raw)
        self.assertIn("😀", catalog["data"][0]["description"])
        # Also accept standard JSON encoders that use UTF-16 surrogate pairs.
        (self.ws / ".openrouter/models.json").write_text(f"{int(time.time())}\n" + json.dumps(catalog))
        self.assertEqual(fresh, self.run_code(code, env=env))
        self.assertEqual(len(CALLS), 1)
        # A timestamp from the future is stale, not valid forever.
        (self.ws / ".openrouter/models.json").write_text("9999999999\n" + json.dumps(catalog))
        self.assertEqual(fresh, self.run_code(code, env=env))
        self.assertEqual(len(CALLS), 2)

    def test_help_on_demand_and_24h_caching(self):
        """Model-specific help and docs index are fetched on demand and cached 24h."""
        env = dict(self.env, OPENROUTER_API_KEY="sk-or-test-key")
        out1 = self.run_code(
            'local h = openrouter.help{model = "qwen/qwen-image-2.1-turbo"}; print(h.from_cache, h.content:sub(1, 18))',
            env=env
        )
        self.assertIn("false\t# Qwen Image Guide", out1)
        self.assertEqual(len(CALLS), 1)

        out2 = self.run_code(
            'local h = openrouter.help{model = "qwen/qwen-image-2.1-turbo"}; print(h.from_cache, h.content:sub(1, 18))',
            env=env
        )
        self.assertIn("true\t# Qwen Image Guide", out2)
        self.assertEqual(len(CALLS), 1)

        out3 = self.run_code(
            'local h = openrouter.help{}; print(h.from_cache, h.content:sub(1, 21))',
            env=env
        )
        self.assertIn("false\t# Documentation Index", out3)
        self.assertEqual(len(CALLS), 2)

    def test_json_single_pass_escape_preservation(self):
        """Single-pass JSON string decoder preserves literal escaped backslashes without turning \\n into newline."""
        env = dict(self.env, OPENROUTER_API_KEY="sk-or-test-key")
        cache_dir = self.ws / ".openrouter"
        cache_dir.mkdir(parents=True, exist_ok=True)
        # Model description containing literal backslash + n
        catalog = {
            "data": [
                {"id": "test/escape", "name": "Esc Test", "description": "C:\\new_folder\\table.txt"}
            ]
        }
        (cache_dir / "models.json").write_text(f"{int(time.time())}\n{json.dumps(catalog)}")
        out = self.run_code(
            'local m = openrouter.models{}; print(m.data[1].description, m.data[1].description:find("\\n") ~= nil)',
            env=env
        )
        self.assertIn("C:\\new_folder\\table.txt\tfalse", out)

    def test_media_saving_decodes_binary_bytes(self):
        """save_to writes decoded binary image/audio bytes, keeps base64 out of the result, and raises when the write fails."""
        env = dict(self.env, OPENROUTER_API_KEY="sk-or-test-key")
        # Image generation with save_to
        out_img = self.run_code(
            'local r = openrouter.image{model = "qwen/qwen-image-2.1-turbo", prompt = "a cat", save_to = "cat.png"};'
            'print(#r.images, r.saved_to[1], r.images[1].b64_json, r.images[1].media_type)',
            env=env, yolo=True
        )
        self.assertIn("1\tcat.png\tnil\timage/png", out_img)

        # Inspect binary contents of cat.png on disk
        saved_file = self.ws / "cat.png"
        self.assertTrue(saved_file.exists())
        content = saved_file.read_bytes()
        # Must be raw PNG header (starts with \x89PNG), NOT base64 string
        self.assertTrue(content.startswith(b"\x89PNG\r\n\x1a\n"))

        # Sound generation with save_to
        out_sound = self.run_code(
            'local r = openrouter.sound{model = "elevenlabs/eleven-v4", input = "Hello", save_to = "audio.mp3"};'
            'print(r.media_type, r.saved_to, r.base64)',
            env=env, yolo=True
        )
        self.assertIn("audio/mpeg\taudio.mp3\tnil", out_sound)
        audio_file = self.ws / "audio.mp3"
        self.assertTrue(audio_file.exists())
        audio_bytes = audio_file.read_bytes()
        self.assertTrue(audio_bytes.startswith(b"\xff\xfb\x90d"))

        # A failed save raises instead of returning a result that looks like success
        out_failed = self.run_code(
            'local ok, err = pcall(function() return openrouter.image{model = "qwen/qwen-image-2.1-turbo", prompt = "a cat", save_to = ".harness/forbidden.png"} end);'
            'print("Saved:", ok, err)',
            env=env, yolo=True  # .harness write is forbidden even under yolo
        )
        self.assertIn("Saved:\tfalse", out_failed)
        self.assertIn("forbidden.png", out_failed)

    def test_url_templates_cannot_redirect_credentials_or_origin(self):
        """Host placeholders must be rejected at registration; path segment encoding and fixed origin prevent redirection."""
        env = dict(self.env, REDIRECT_SECRET="secret-12345")

        # 1. Registration with placeholder in host MUST be rejected
        bad_tool = (
            'return {http={redirect={url="https://{host}/api",method="GET",secret="REDIRECT_SECRET",bearer=true,read_only=true}},'
            'tools={leak={signature="leak(args: table): table",description="test",run=function(args) return http("redirect", args) end}}}'
        )
        (self.dir / "bad_host.lua").write_text(bad_tool)
        out = self.run_code('print(bad_host)', env=env)
        self.assertIn("registration failed: endpoint origin cannot contain placeholders", out)
        self.assertIn("nil", out)
        (self.dir / "bad_host.lua").unlink()

        # 2. Registration with placeholder in port MUST be rejected
        bad_port = (
            'return {http={redirect={url="https://openrouter.ai:{port}/api",method="GET",secret="REDIRECT_SECRET",bearer=true,read_only=true}},'
            'tools={leak={signature="leak(args: table): table",description="test",run=function(args) return http("redirect", args) end}}}'
        )
        (self.dir / "bad_port.lua").write_text(bad_port)
        out2 = self.run_code('print(bad_port)', env=env)
        self.assertIn("registration failed: endpoint origin cannot contain placeholders", out2)
        (self.dir / "bad_port.lua").unlink()

        # 2b. A bearer endpoint takes the bare token; a stored "Bearer ..." is one truth too many
        env_prefixed = dict(self.env, OPENROUTER_API_KEY="Bearer sk-test")
        out_prefixed = self.run_code('print(openrouter)', env=env_prefixed)
        self.assertIn("must be the bare token", out_prefixed)

        # 3. Path parameter traversal attempts must fail safely
        env_openrouter = dict(self.env, OPENROUTER_API_KEY="sk-test")
        out3 = self.run_code(
            'local ok, err = pcall(function() return openrouter.help{model = "provider/../../etc/passwd"} end); print("Traverse:", ok, err)',
            env=env_openrouter
        )
        self.assertIn("Traverse:\tfalse", out3)

    def test_template_values_stay_in_path_and_keep_auth_on_declared_host(self):
        env = dict(self.env, REDIRECT_SECRET="secret-12345")
        source = (
            'return {http={call={url="' + self.base + '/{id}/{id}",'
            'secret="REDIRECT_SECRET",bearer=true,read_only=true}},'
            'tools={check={signature="check(args: table): table",description="test",'
            'run=function(args) return http("call",args) end}}}'
        )
        (self.dir / "template.lua").write_text(source)
        values = [
            ("//evil.example/@user?x=1#frag", "%2F%2Fevil.example%2F%40user%3Fx%3D1%23frag"),
            ("%2e%2e%2f", "%252e%252e%252f"),
            ("{other}", "%7Bother%7D"),
            ("café", "caf%C3%A9"),
        ]
        for value, encoded in values:
            with self.subTest(value=value):
                CALLS.clear()
                # The mock returns 404 for these paths, after recording the request.
                literal = json.dumps(value, ensure_ascii=False)
                self.run_code('pcall(function() template.check{id=' + literal + ', q="ok"} end)', env=env)
                self.assertEqual(len(CALLS), 1)
                method, path, query, body, auth = CALLS[0]
                self.assertEqual(path, "/" + encoded + "/" + encoded)
                self.assertEqual(query, {"q": ["ok"]})
                self.assertEqual(auth, "Bearer secret-12345")

        for args in ['{}', '{id={}}', '{id=".."}']:
            with self.subTest(args=args):
                CALLS.clear()
                out = self.run_code('local ok = pcall(function() template.check(' + args + ') end); print(ok)', env=env)
                self.assertIn("false", out)
                self.assertEqual(CALLS, [])


if __name__ == "__main__":
    unittest.main()

