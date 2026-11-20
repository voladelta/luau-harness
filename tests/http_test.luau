local loop = assert(host.load(host.default_loop, "=loop"))()
local dir = host.env("TMPDIR") .. "/"

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, has, raises = checks.check, checks.has, checks.raises

local http = loop.ext("http")

check("the system prompt lists http", loop.system_prompt():find("- http (built in): HTTP requests", 1, true) ~= nil, true)

host.write(dir .. "server.py", [==[
import http.server, sys, time

class Handler(http.server.BaseHTTPRequestHandler):
    def reply(self, status, body, headers=()):
        self.send_response(status)
        for name, value in headers:
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/ok":
            self.reply(200, b"hello", [("X-Twice", "a"), ("X-Twice", "b")])
        elif self.path == "/moved":
            self.reply(302, b"", [("Location", "/ok")])
        elif self.path == "/slow":
            time.sleep(1)
            self.reply(200, b"late")
        else:
            self.reply(404, b"no such page")

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        self.reply(201, self.command.encode() + b" " + self.headers["Content-Type"].encode() + b" " + body)

    do_PUT = do_POST

    def do_HEAD(self):
        self.send_response(204)
        self.send_header("X-Head", "yes")
        self.end_headers()

    def log_message(self, *args):
        pass

server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
open(sys.argv[1], "w").write(str(server.server_port))
server.serve_forever()
]==])

host.spawn(function()
	host.sh({ "python3", dir .. "server.py", dir .. "port" }, { timeout = 60 })
	return ""
end)
local port
for _ = 1, 100 do
	port = host.read(dir .. "port")
	if port and port ~= "" then
		break
	end
	host.sleep(0.05)
end
local base = "http://127.0.0.1:" .. port

local ok = http.request(base .. "/ok")
check("a GET's status", ok.status, 200)
check("its body", ok.body, "hello")
check("a header named twice is joined", ok.headers["x-twice"], "a, b")
check("names are lower case", ok.headers["content-length"], "5")

local moved = http.request(base .. "/moved")
check("a redirect is followed", moved.status, 200)
check("to its body", moved.body, "hello")

local missing = http.request(base .. "/missing")
check("an error status is returned, not raised", missing.status, 404)
check("with the body it came with", missing.body, "no such page")

local posted = http.request(base .. "/echo", { json = { a = 1 } })
check("json is posted as JSON", posted.body, 'POST application/json {"a":1}')
check("and its status", posted.status, 201)

local custom_ct = http.request(base .. "/echo", {
	json = { a = 1 },
	headers = { ["content-Type"] = "application/merge-patch+json" },
})
check("mixed-case Content-Type is kept for json requests", custom_ct.body, 'POST application/merge-patch+json {"a":1}')

local put = http.request(base .. "/echo", { method = "PUT", body = "it's \"raw\"\n", headers = { ["Content-Type"] = "text/plain" } })
check("a body goes as it is, with the headers given", put.body, "PUT text/plain it's \"raw\"\n")

local head = http.request(base .. "/ok", { method = "head", timeout = 5 })
check("a HEAD request ends without a body", head.status, 204)
check("with its headers", head.headers["x-head"], "yes")
check("and no body", head.body, "")

local _, err = pcall(http.request, "http://127.0.0.1:1/")
has("a failed connection raises", err, "http.request http://127.0.0.1:1/: curl:")
local _, slow_err = pcall(http.request, base .. "/slow", { timeout = 0.2 })
has("a request past its timeout raises what curl says", slow_err, "http.request " .. base .. "/slow: curl: (28)")
host.sh("(for _ in $(seq 50); do pkill -KILL -f '[c]url .*/slow' && break; sleep 0.1; done) >/dev/null 2>&1 &")
local _, killed_err = pcall(http.request, base .. "/slow", { timeout = 5 })
check("a curl that dies without a word is still explained", killed_err,
	"http.request " .. base .. "/slow: killed by signal 9")
raises("body and json both", "give body or json, not both", http.request, base, { body = "x", json = {} })
raises("a url that is not text", "http.request: url must be a string, not nil", http.request)
check("nothing is left in TMPDIR", #host.walk(dir, { glob = "tmp.*" }), 0)

checks.done("http")
