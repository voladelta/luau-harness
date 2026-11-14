-- searches the web and fetches pages, through DeepSeek: search(query, {count?, content?, question?}) -> results, fetch(url) -> markdown
--
-- Built into hx; a project's own .hx/ext/web.luau takes its place. Both calls go to DeepSeek's Search and Fetch
-- APIs with DEEPSEEK_API_KEY, whatever model the loop talks to. hx adds the key; this code never sees it.

local M = {}

M.attempts = 3 -- a request that fails with 408, 429 or 5xx, or gets no answer, is tried this many times
M.timeout = 60 -- seconds per request
M.summary_limit = 800 -- characters of a result's summary shown when results are printed; the field keeps all of it

local function note(text)
	if not host.quiet then
		host.log(text)
	end
end

-- The message of an error body in the OpenAI format, {"error": {"message": ...}}, or the start of the text.
local function message(text)
	local ok, body = pcall(json.decode, text or "")
	if ok and type(body) == "table" then
		local err = body.error
		if type(err) == "table" and type(err.message) == "string" then
			return err.message
		elseif type(err) == "string" then
			return err
		end
	end
	return tostring(text):sub(1, 300)
end

-- Posts a request to DeepSeek's tool endpoint `name` and returns the decoded answer. Retries what is worth
-- retrying, then raises.
local function call(name, request)
	if not host.tools then
		error(name .. ": hx was started without DEEPSEEK_API_KEY, so DeepSeek's " .. name .. " cannot be used", 0)
	end
	local body = json.encode(request)
	for attempt = 1, M.attempts do
		local status, text = host.tool(name, body, { timeout = M.timeout })
		if status == 200 then
			local ok, value = pcall(json.decode, text)
			if not ok or type(value) ~= "table" then
				error(("%s: DeepSeek sent an answer that is not JSON: %s"):format(name, tostring(text):sub(1, 300)), 0)
			end
			if value.error then
				error(("%s failed: %s"):format(name, message(text)), 0)
			end
			return value
		end
		local transient = type(status) ~= "number" or status == 408 or status == 429 or status >= 500
		if not transient or attempt == M.attempts then
			error(("%s failed (%s): %s"):format(name, tostring(status or "no answer"), message(text)), 0)
		end
		local wait = 2 ^ attempt
		note(("  %s failed (%s), retrying in %d s"):format(name, tostring(status or text), wait))
		host.sleep(wait)
	end
	error("unreachable")
end

local function check(fname, value, position, name, kind, optional)
	if type(value) == kind or value == nil and optional then
		return
	end
	error(("web.%s: argument %d (%s) must be a %s, not %s"):format(fname, position, name, kind, type(value)), 0)
end

-- What printing a list of search results shows: one block per result, best first.
local function results_text(results)
	if #results == 0 then
		return "[no results]"
	end
	local blocks = {}
	for i, r in results do
		local about = {}
		for _, part in { r.site_name, r.date and r.date:sub(1, 10) } do
			if part and part ~= "" then
				table.insert(about, part)
			end
		end
		local lines = { ("%d. %s"):format(i, r.title) }
		if #about > 0 then
			lines[1] ..= " (" .. table.concat(about, ", ") .. ")"
		end
		table.insert(lines, "   " .. r.url)
		local text = (r.summary ~= "" and r.summary or r.snippet):gsub("%s+", " ")
		if #text > M.summary_limit then -- the whole summary stays in the result
			local cut = utf8.offset(text, M.summary_limit + 1) or #text + 1
			text = text:sub(1, cut - 1) .. " ..."
		end
		if text ~= "" then
			table.insert(lines, "   " .. text)
		end
		if r.content and r.content ~= "" then
			table.insert(lines, "   content:\n" .. r.content)
		end
		table.insert(blocks, table.concat(lines, "\n"))
	end
	return table.concat(blocks, "\n\n")
end
local Results = { __tostring = results_text }

-- search(query, {count = 10, content = false, question = nil}) -> results
--
-- query takes site:, -site:, lang:, after:, before: and "exact phrases", and is at most 128 characters. count is
-- 1 to 50. content = true asks for each page's text too. question is the user's own words, which helps ranking.
-- The results are a list, best first, of {title, url, snippet, summary, content?, site_name?, date?, rerank_score
-- (0-3; 1.5 and up is good), authority (-1 unknown, 3 and up is authoritative)}. Printed, they read as text.
function M.search(query, options)
	check("search", query, 1, "query", "string")
	check("search", options, 2, "options", "table", true)
	options = options or {}
	local length = utf8.len(query)
	if not length or query:match("^%s*$") or length > 128 then
		error("web.search: the query must be 1 to 128 characters and not all spaces", 0)
	end
	local count = options.count or 10
	if type(count) ~= "number" or count < 1 or count > 50 or count % 1 ~= 0 then
		error("web.search: count must be a whole number from 1 to 50", 0)
	end
	local question = options.question
	if question ~= nil then
		check("search", question, 2, "options.question", "string")
		if not utf8.len(question) then
			question = nil -- not UTF-8: it would only get the whole search refused
		elseif utf8.len(question) > 128 then
			question = question:sub(1, utf8.offset(question, 129) - 1) -- the API takes 128 characters
		end
	end
	note("  search " .. query:gsub("%c", " "))
	local answer = call("search", {
		query = query,
		count = count,
		need_content = options.content and true or nil,
		ori_question = question ~= "" and question or nil,
	})
	local results = {}
	for _, r in type(answer.results) == "table" and answer.results or {} do
		table.insert(results, {
			title = r.title or "",
			url = r.url or "",
			snippet = r.snippet or "",
			summary = r.summary or "",
			content = r.content ~= "" and r.content or nil,
			site_name = r.site_name,
			date = r.date,
			rerank_score = r.rerank_score,
			authority = r.authority,
		})
	end
	return setmetatable(results, Results)
end

-- fetch(url) -> text: the page as Markdown. When the page has a title that its first lines do not already show,
-- it comes first as a heading.
function M.fetch(url)
	check("fetch", url, 1, "url", "string")
	if not url:match("^[Hh][Tt][Tt][Pp][Ss]?://[^/%s]") or #url > 8192 then
		error("web.fetch: the url must start with http:// or https://, name a host, and be at most 8192 bytes", 0)
	end
	note("  fetch " .. url:gsub("%c", ""))
	local page = call("fetch", { url = url })
	local title = type(page.title) == "string" and page.title or ""
	local content = type(page.content) == "string" and page.content or ""
	if title ~= "" and not content:sub(1, 500):find(title, 1, true) then
		content = "# " .. title .. "\n\n" .. content
	end
	return content ~= "" and content or "[the page has no content]"
end

return M
