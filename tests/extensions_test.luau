local loop = assert(host.load(host.default_loop, "=loop"))()

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, raises = checks.check, checks.raises

assert(not host.stat(".hx"), "extensions_test writes .hx and removes it: run it from a scratch directory, not from a project")

local calls, answers, waits = {}, {}, {}
host.post = function(name, request, options)
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

host.endpoints = { exa = false, contents = false }
raises("search without a key", "without EXA_API_KEY", loop.ext("web").search, "luau")
raises("fetch without a key", "without EXA_API_KEY", loop.ext("web").fetch, "https://luau.org")
raises("exa without a key", "without EXA_API_KEY", loop.ext("exa").deep, "luau", "why")
check("no request without a key", #calls, 0)
host.endpoints = { exa = true, contents = true }

local web = loop.ext("web")
check("it is loaded once", loop.ext("web"), web)

script({ 200, json.encode({ results = {
	{ title = "Luau", url = "https://luau.org", publishedDate = "2026-06-01T00:00:00Z", author = "Roblox",
		highlights = { "A fast,\nsmall language.", 7, "Gradually typed." } },
	{ title = "Other", url = "https://example.com", author = "", text = "" },
} }) })
local results = web.search("luau language", { count = 2 })
check("search asks Exa's search endpoint", calls[1].name, "exa")
local sent = json.decode(calls[1].request)
check("search sends the query", sent.query, "luau language")
check("search sends the count", sent.numResults, 2)
check("search lets Exa pick how to search", sent.type, "auto")
check("search asks for the passages that matter", sent.contents.highlights, true)
check("search does not ask for content unasked", sent.contents.text, nil)
check("search returns the results", #results, 2)
check("a result keeps its url", results[1].url, "https://luau.org")
check("a highlight that is not text is left out", #results[1].highlights, 2)
check("empty content is left out", results[2].content, nil)
check("missing fields stay missing", results[2].date, nil)
check("results read as text", tostring(results), table.concat({
	"1. Luau (Roblox, 2026-06-01)",
	"   https://luau.org",
	"   A fast, small language. ... Gradually typed.",
	"",
	"2. Other",
	"   https://example.com",
}, "\n"))
script({ 200, '{"results": []}' })
check("the tool shows results as text", loop.tools[1].run({ code = "return ext('web').search('x')" }), "[no results]")

script({ 200, json.encode({ results = { { title = "T", url = "u", highlights = { string.rep("é", 900) } } } }) })
local long = web.search("q")
check("long highlights are cut when shown", tostring(long):find(string.rep("é", 800) .. " ...", 1, true) ~= nil, true)
check("but kept whole in the result", utf8.len(long[1].highlights[1]), 900)

script({ 200, json.encode({ results = { { title = "T", url = "u", highlights = { "s" }, text = "body" } } }) })
check("content is shown when asked for", tostring(web.search("q", { content = true })):find("content:\nbody", 1, true) ~= nil, true)
check("content is asked for, up to a limit", json.decode(calls[1].request).contents.text.maxCharacters, 20000)

raises("an empty query", "1 to 2000 bytes", web.search, "   ")
raises("a long query", "1 to 2000 bytes", web.search, string.rep("a", 2001))
raises("a bad count", "count must be", web.search, "q", { count = 51 })
raises("a query that is not a string", "must be a string, not table", web.search, { query = "q" })

script({ 503, '{"error": "busy"}' }, { 429, "slow down" }, { 200, '{"results": []}' })
web.search("q")
check("503 and 429 are retried", #calls, 3)
check("after 2 s, then 4", table.concat(waits, ","), "2,4")
check("with the time limit each", calls[1].options.timeout, 60)
script({ 400, '{"requestId": "r", "error": "bad query", "tag": "INVALID_REQUEST_BODY"}' })
raises("a 400", "web.search failed (400): bad query", web.search, "q")
check("a 400 is not retried", #calls, 1)
script({ nil, "Could not resolve host" }, { nil, "Could not resolve host" }, { nil, "Could not resolve host" })
raises("no answer at all", "web.search failed (no answer): Could not resolve host", web.search, "q")
check("no answer is retried", #calls, 3)
script({ 200, "<html>" })
raises("an answer that is not JSON", "not JSON", web.search, "q")
script({ 404, string.rep("x", 400) })
check("an error page is shown cut to 300 bytes", select(2, pcall(web.search, "q")), "web.search failed (404): " .. string.rep("x", 300))
script({ 200, '{"results":[{"title":"T","url":"u","highlights":["' .. string.rep("\255", 900) .. '"]}]}' })
check("a highlight that is not UTF-8 is cut by bytes", tostring(web.search("q")), "1. T\n   u\n   " .. string.rep("\255", 800) .. " ...")

script({ 200, json.encode({ results = { { title = "Your First API Call", url = "https://example.com/a",
	text = "# Your First API Call\n\nHello." } }, statuses = { { id = "https://example.com/a", status = "success" } } }) })
check("fetch returns the content", web.fetch("https://example.com/a"), "# Your First API Call\n\nHello.")
check("fetch asks Exa's contents endpoint", calls[1].name, "contents")
check("fetch sends the url", json.decode(calls[1].request).ids[1], "https://example.com/a")
check("and asks for the whole text", json.decode(calls[1].request).text, true)
script({ 200, json.encode({ results = { { title = "Title", text = "Body." } } }) })
check("a title the page does not show comes first", web.fetch("http://example.com"), "# Title\n\nBody.")
script({ 200, json.encode({ results = { { title = "", text = "" } } }) })
check("an empty page says so", web.fetch("http://example.com"), "[the page has no content]")
script({ 200, '{"results": [], "statuses": [{"id": "http://example.com", "status": "error", '
	.. '"error": {"tag": "CRAWL_NOT_FOUND", "httpStatusCode": 404}}]}' })
raises("a page Exa cannot read", "Exa could not read http://example.com: CRAWL_NOT_FOUND", web.fetch, "http://example.com")
check("which is not asked for again", #calls, 1)
raises("fetch of a file url", "http:// or https://", web.fetch, "file:///etc/passwd")
raises("fetch of no host", "http:// or https://", web.fetch, "https://")

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

host.write(".hx/ext/web.luau", "-- mine\nreturn { search = function() return 'own' end }\n")
check("the project's own web takes the built-in one's place", select(2, pcall(function()
	return loop.ext("web").search("q")
end)), "own")
host.sh("rm .hx/ext/web.luau")
check("and the built-in one is back when the file is gone", type(loop.ext("web").fetch), "function")

host.write(".hx/ext/noisy.luau", "-- prints\nreturn { f = function() print('EXT-PRINT', 1) return 'value' end }\n")
check("an extension's print reaches the tool result",
	loop.tools[1].run({ code = "print('before') local v = ext('noisy').f() print('after') return v" }),
	"before\nEXT-PRINT\t1\nafter\nvalue")
host.sh("rm -rf .hx")

checks.done("extensions")
