-- searches the web and fetches pages, through DeepSeek: search(query, {count?, content?, question?}) -> results, fetch(url) -> markdown
--!strict
--
-- Built into hx; a project's own .hx/ext/web.luau takes its place. Both calls go to DeepSeek's Search and Fetch
-- APIs with DEEPSEEK_API_KEY, whatever model the loop talks to. hx adds the key; this code never sees it.

local http = ext("_http")
local note, check = http.note, http.check

local M = {}

M.attempts = 3 -- a request that fails with 408, 429 or 5xx, or gets no answer, is tried this many times
M.timeout = 60 -- seconds per request
M.summary_limit = 800 -- characters of a result's summary shown when results are printed; the field keeps all of it

-- One search result, as search returns it.
type Result = {
	title: string,
	url: string,
	snippet: string,
	summary: string,
	content: string?,
	site_name: string?,
	date: string?,
	rerank_score: number?,
	authority: number?,
}

type SearchOptions = { count: number?, content: boolean?, question: string? }

-- Posts a request to DeepSeek's tool endpoint `name` and returns the decoded answer. Retries what is worth
-- retrying, then raises.
local function call(name: string, request: { [string]: unknown }): { [string]: any }
	if not host.tools then
		error(name .. ": hx was started without DEEPSEEK_API_KEY, so DeepSeek's " .. name .. " cannot be used", 0)
	end
	return http.call({
		endpoint = name,
		body = json.encode(request),
		attempts = M.attempts,
		timeout = M.timeout,
		label = name,
		what = name,
		sender = "DeepSeek",
		body_error = {}, -- an answer that carries an error failed, and another try would not change it
	})
end

-- What printing a list of search results shows: one block per result, best first.
local function results_text(results: { Result }): string
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
		-- The whole summary stays in the result.
		local text: string = http.clip(r.summary ~= "" and r.summary or r.snippet, M.summary_limit)
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
function M.search(query: string, options: SearchOptions?)
	check("web.search", query, 1, "query", "string")
	check("web.search", options, 2, "options", "table", true)
	local given: SearchOptions = options or {}
	local length = utf8.len(query)
	if not length or query:match("^%s*$") or length > 128 then
		error("web.search: the query must be 1 to 128 characters and not all spaces", 0)
	end
	local count = given.count or 10
	if type(count) ~= "number" or count < 1 or count > 50 or count % 1 ~= 0 then
		error("web.search: count must be a whole number from 1 to 50", 0)
	end
	local question = given.question
	if question ~= nil then
		check("web.search", question, 2, "options.question", "string")
		local characters = utf8.len(question)
		if not characters then
			question = nil -- not UTF-8: it would only get the whole search refused
		elseif characters > 128 then
			-- The API takes 128 characters. A text of more than 128 has a 129th, so utf8.offset finds it.
			question = question:sub(1, (utf8.offset(question, 129) :: number) - 1)
		end
	end
	note("  search " .. query:gsub("%c", " "))
	local answer = call("search", {
		query = query,
		count = count,
		need_content = given.content and true or nil,
		ori_question = question ~= "" and question or nil,
	})
	local results: { Result } = {}
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
function M.fetch(url: string): string
	check("web.fetch", url, 1, "url", "string")
	if not url:match("^[Hh][Tt][Tt][Pp][Ss]?://[^/%s]") or #url > 8192 then
		error("web.fetch: the url must start with http:// or https://, name a host, and be at most 8192 bytes", 0)
	end
	note("  fetch " .. url:gsub("%c", ""))
	local page = call("fetch", { url = url })
	local title: string = type(page.title) == "string" and page.title or ""
	local content: string = type(page.content) == "string" and page.content or ""
	if title ~= "" and not content:sub(1, 500):find(title, 1, true) then
		content = "# " .. title .. "\n\n" .. content
	end
	return content ~= "" and content or "[the page has no content]"
end

return M
