-- The built-in extensions, web, ocr and exa, and the helper they share, against a stand-in for host.tool.
-- Run with: hx --run tests/extensions_test.luau

local loop = assert(host.load(host.default_loop, "=loop"))()

local failures = 0
local function check(name, got, want)
	if got ~= want then
		failures += 1
		print(("FAIL %s\n  got:  %s\n  want: %s"):format(name, tostring(got), tostring(want)))
	end
end

local function raises(name, fragment, fn, ...)
	local ok, err = pcall(fn, ...)
	check(name .. " raises", ok, false)
	if not ok and not tostring(err):find(fragment, 1, true) then
		check(name .. " message", err, fragment)
	end
end

-- The stand-in answers each call with the next scripted {status, text} and records what it was asked.
-- The seconds of each wait between tries are recorded too, instead of slept.
local calls, answers, waits = {}, {}, {}
host.tool = function(name, request, options)
	table.insert(calls, { name = name, request = request, options = options })
	local answer = table.remove(answers, 1) or { 500, "script ran out" }
	return answer[1], answer[2]
end
host.sleep = function(seconds)
	table.insert(waits, seconds)
end
host.quiet = true
local function script(...)
	table.clear(calls)
	table.clear(waits)
	answers = { ... }
end

-- Without the key, the built-in extensions load but say what is missing.
host.tools = false
raises("search without a key", "without DEEPSEEK_API_KEY", loop.ext("web").search, "luau")
raises("ocr without a key", "without DEEPSEEK_API_KEY", loop.ext("ocr").read, "a.pdf")
check("no request without a key", #calls, 0)
host.tools = true

local web, ocr = loop.ext("web"), loop.ext("ocr")
check("the web extension loads", type(web.search), "function")
check("it is loaded once", loop.ext("web"), web)

-- search
script({ 200, json.encode({ results = {
	{ rerank_score = 2.4, title = "Luau", url = "https://luau.org", snippet = "short", summary = "A fast,\nsmall language.",
		content = "", site_name = "luau.org", date = "2026-06-01T00:00:00Z", authority = 3 },
	{ rerank_score = 0.9, title = "Other", url = "https://example.com", snippet = "just a snippet", summary = "",
		content = "", authority = -1 },
} }) })
local results = web.search("luau language", { count = 2, question = "what is luau?" })
check("search asks the search endpoint", calls[1].name, "search")
local sent = json.decode(calls[1].request)
check("search sends the query", sent.query, "luau language")
check("search sends the count", sent.count, 2)
check("search sends the question", sent.ori_question, "what is luau?")
check("search does not ask for content unasked", sent.need_content, nil)
check("search returns the results", #results, 2)
check("a result keeps its url", results[1].url, "https://luau.org")
check("a result keeps its score", results[1].rerank_score, 2.4)
check("empty content is left out", results[1].content, nil)
check("missing fields stay missing", results[2].site_name, nil)
check("results read as text", tostring(results), table.concat({
	"1. Luau (luau.org, 2026-06-01)",
	"   https://luau.org",
	"   A fast, small language.",
	"",
	"2. Other",
	"   https://example.com",
	"   just a snippet",
}, "\n"))
-- The lua tool shows such a value as it reads, not as JSON.
script({ 200, '{"results": []}' })
check("the tool shows results as text", loop.tools[1].run({ code = "return ext('web').search('x')" }), "[no results]")

script({ 200, json.encode({ results = { { title = "T", url = "u", snippet = "", summary = string.rep("é", 900) } } }) })
local long = web.search("q")
check("a long summary is cut when shown", tostring(long):find(string.rep("é", 800) .. " ...", 1, true) ~= nil, true)
check("but kept whole in the result", utf8.len(long[1].summary), 900)

script({ 200, json.encode({ results = { { title = "T", url = "u", snippet = "", summary = "s", content = "body" } } }) })
check("content is shown when asked for", tostring(web.search("q", { content = true })):find("content:\nbody", 1, true) ~= nil, true)
check("content is asked for", json.decode(calls[1].request).need_content, true)

-- A long question is cut to 128 characters, on a character boundary.
script({ 200, '{"results": []}' })
web.search("q", { question = string.rep("é", 200) })
check("the question is cut to 128 characters", utf8.len(json.decode(calls[1].request).ori_question), 128)

raises("an empty query", "1 to 128 characters", web.search, "   ")
raises("a long query", "1 to 128 characters", web.search, string.rep("a", 129))
raises("a bad count", "count must be", web.search, "q", { count = 51 })
raises("a query that is not a string", "must be a string, not table", web.search, { query = "q" })

-- Transient failures are retried; others are not.
script({ 503, '{"error": {"message": "busy"}}' }, { 429, "slow down" }, { 200, '{"results": []}' })
web.search("q")
check("503 and 429 are retried", #calls, 3)
check("after 2 s, then 4", table.concat(waits, ","), "2,4")
check("with the time limit each", calls[1].options.timeout, 60)
script({ 200, '{"error": {"message": "quota spent"}}' })
raises("an answer that carries an error", "search failed: quota spent", web.search, "q")
check("such an answer is not retried", #calls, 1)
script({ 400, '{"error": {"message": "bad query", "type": "invalid_request_error"}}' })
raises("a 400", "search failed (400): bad query", web.search, "q")
check("a 400 is not retried", #calls, 1)
script({ nil, "Could not resolve host" }, { nil, "Could not resolve host" }, { nil, "Could not resolve host" })
raises("no answer at all", "search failed (no answer): Could not resolve host", web.search, "q")
check("no answer is retried", #calls, 3)
script({ 200, "<html>" })
raises("an answer that is not JSON", "not JSON", web.search, "q")

-- fetch
script({ 200, json.encode({ title = "Your First API Call", content = "# Your First API Call\n\nHello." }) })
check("fetch returns the content", web.fetch("https://example.com/a"), "# Your First API Call\n\nHello.")
check("fetch asks the fetch endpoint", calls[1].name, "fetch")
check("fetch sends the url", json.decode(calls[1].request).url, "https://example.com/a")
script({ 200, json.encode({ title = "Title", content = "Body." }) })
check("a title the page does not show comes first", web.fetch("http://example.com"), "# Title\n\nBody.")
script({ 200, json.encode({ title = "", content = "" }) })
check("an empty page says so", web.fetch("http://example.com"), "[the page has no content]")
raises("fetch of a file url", "http:// or https://", web.fetch, "file:///etc/passwd")
raises("fetch of no host", "http:// or https://", web.fetch, "https://")

-- ocr
check("a pdf keeps its layout", ocr.purpose("docs/Scan.PDF"), "rich_layout_recognition")
check("an image is plain text", ocr.purpose("photo.png"), "text_recognition")
check("a word file is plain text", ocr.purpose("notes.docx"), "text_recognition")

script({ 200, "\n\n\n" .. json.encode({ id = "ocr-1", object = "ocr.result", page_count = 2, pages = {
	{ index = 1, text = "second", finish_reason = "length" },
	{ index = 0, text = "first", finish_reason = "stop" },
} }) })
check("ocr returns the pages in order", ocr.read("scan.pdf"),
	"[page 1]\nfirst\n\n[page 2]\nsecond\n[this page was cut off: the recognition reached its output limit]")
check("ocr asks the ocr endpoint", calls[1].name, "ocr")
check("ocr sends the file as a path", calls[1].request.file, "scan.pdf")
check("ocr reads a pdf with its layout", calls[1].request.purpose, "rich_layout_recognition")

script({ 200, json.encode({ page_count = 1, pages = { { index = 0, text = "hello", finish_reason = "stop" } } }) })
check("one page has no page line", ocr.read("photo.jpg"), "hello")
check("ocr reads an image as text", calls[1].request.purpose, "text_recognition")

script({ 200, json.encode({ page_count = 2, pages = { { index = 0, text = " \n", finish_reason = "stop" },
	{ index = 1, text = "b", finish_reason = "stop" } } }) })
check("an empty page is marked", ocr.read("two.pdf"), "[page 1]\n[no text was recognized on this page]\n\n[page 2]\nb")

script({ 200, json.encode({ page_count = 1, pages = { { index = 0, text = "x", finish_reason = "stop" } } }) })
ocr.read("scan.pdf", "text_recognition")
check("the caller's purpose wins", calls[1].request.purpose, "text_recognition")
raises("a purpose that does not exist", "purpose must be", ocr.read, "scan.pdf", "layout")

script({ 200, json.encode({ error = { message = "the file is password protected", type = "invalid_request_error" } }) })
raises("a failed recognition", "password protected", ocr.read, "locked.pdf")
check("a failed recognition is not retried", #calls, 1)
script({ 200, json.encode({ error = { message = "try later", type = "service_unavailable" } }) },
	{ 200, json.encode({ page_count = 1, pages = { { index = 0, text = "ok", finish_reason = "stop" } } }) })
check("an unavailable service is tried again", ocr.read("a.png"), "ok")
check("it took two calls", #calls, 2)
check("ocr waits 4 s before its second try", table.concat(waits, ","), "4")
script({ 200, json.encode({ error = { message = "down", code = "internal_error" } }) },
	{ 200, json.encode({ error = { message = "down", code = "internal_error" } }) },
	{ 200, json.encode({ error = { message = "still down", code = "internal_error" } }) })
raises("a recognition that keeps failing", "ocr of a.png failed (recognition failed): still down", ocr.read, "a.png")
check("and 8 s before its third", table.concat(waits, ","), "4,8")
script({ 413, '{"error": {"message": "file too large"}}' })
raises("a file too large", "(413): file too large", ocr.read, "big.pdf")
raises("ocr of a path that is not a string", "ocr.read: argument 1 (path) must be a string, not number", ocr.read, 1)

-- exa
host.exa = false
raises("exa without a key", "without EXA_API_KEY", loop.ext("exa").deep, "luau", "why")
host.exa = true
local exa = loop.ext("exa")
script({ 200, json.encode({
	output = { content = "Luau is gradually typed.", grounding = { { field = "content" } } },
	results = { { title = "Luau", url = "https://luau.org", publishedDate = "2026-08-26T00:00:00Z",
		highlights = { "small  and\nfast", 7, "typed" } } },
	notices = { { message = "site: removed." } },
	warnings = { { message = "x is ignored." } },
	costDollars = { total = 0.012 },
}) })
local research = exa.deep("what is luau", "pull the type system", { queries = { "luau types" }, count = 3 })
check("deep asks exa", calls[1].name, "exa")
local asked = json.decode(calls[1].request)
check("deep sends the query", asked.query, "what is luau")
check("deep sends the further queries", asked.additionalQueries[1], "luau types")
check("deep sends the count", asked.numResults, 3)
check("deep asks for a text answer", asked.outputSchema.type, "text")
check("deep keeps the cost", research.cost, 0.012)
check("a highlight that is not text is left out", #research.results[1].highlights, 2)
check("research reads as text", tostring(research), table.concat({
	"Luau is gradually typed.",
	"",
	"notice: site: removed.",
	"",
	"Sources:",
	"1. Luau (2026-08-26)",
	"   https://luau.org",
	"   small and fast ... typed",
}, "\n"))
script({ 401, '{"message": "bad key"}' })
raises("an Exa error with its message at the top", "exa.deep failed (401): bad key", exa.deep, "q", "o")
script({ 500, "" }, { 502, "" }, { 200, '{"results": []}' })
check("exa retries a 5xx", tostring(exa.deep("q", "o")), "[no sources]")
check("exa waits 2 s, then 4", table.concat(waits, ","), "2,4")
script({ 200, "<html>" })
raises("an Exa answer that is not JSON", "exa: Exa sent an answer that is not JSON: <html>", exa.deep, "q", "o")
raises("deep with an objective that is not a string", "exa.deep: argument 2 (objective) must be a string, not nil",
	exa.deep, "q")
raises("a schema that is not an object", 'of type "object"', exa.deep, "q", "o", { schema = { type = "array" } })

-- _http, the helper the three share
local http = loop.ext("_http")
check("an error's message", http.message('{"error": {"message": "m"}}'), "m")
check("an error as text", http.message('{"error": "e"}'), "e")
check("a message at the top is not read unasked", http.message('{"message": "top"}'), '{"message": "top"}')
check("a message at the top, asked for", http.message('{"message": "top"}', true), "top")
check("a body that is not JSON", http.message(string.rep("x", 400)), string.rep("x", 300))
check("no body", http.message(nil), "nil")
check("a short text is not clipped", http.clip("a  b\n c", 10), "a b c")
check("a long text is clipped on a character", http.clip(string.rep("é", 5), 3), "ééé ...")
check("the limit counts characters, not bytes", http.clip(string.rep("é", 500), 800), string.rep("é", 500))
check("a text of exactly the limit is whole", http.clip(string.rep("é", 3), 3), "ééé")
check("text that is not UTF-8 is counted in bytes", http.clip(string.rep("\255", 5), 3), "\255\255\255 ...")
check("and is whole within the limit", http.clip("\255\255", 3), "\255\255")

-- A project's own extension of the same name takes the place of the built-in one. ext() reads .hx/ext in the
-- current directory, the build directory here, so the file is removed again.
local own = ".hx/ext/web.luau"
if not host.read(own) then
	host.write(own, "-- mine\nreturn { search = function() return 'own' end }\n")
	local ok, value = pcall(function()
		return loop.ext("web").search("q")
	end)
	host.sh("rm -f " .. own .. " && rmdir .hx/ext .hx")
	check("the project's own web wins", value, "own")
	check("and loads", ok, true)
end

-- What an extension prints goes to the model, in the tool result, with what the model's code prints; not to
-- stdout, which is the user's answer.
if not host.read(".hx/ext/noisy.luau") then
	host.write(".hx/ext/noisy.luau", "-- prints\nreturn { f = function() print('EXT-PRINT', 1) return 'value' end }\n")
	local code = "print('before') local v = ext('noisy').f() print('after') return v"
	local ok, result = pcall(loop.tools[1].run, { code = code })
	host.sh("rm -f .hx/ext/noisy.luau && rmdir .hx/ext .hx")
	check("an extension's print reaches the tool result", result, "before\nEXT-PRINT\t1\nafter\nvalue")
	check("and the call works", ok, true)
end

if failures > 0 then
	error(("%d check(s) failed"):format(failures), 0)
end
print("extensions: all checks passed")
