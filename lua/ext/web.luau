-- searches the web and reads pages, through Exa: search(query, {count?, content?}) -> results, fetch(url) -> markdown
--!strict

local http = ext("_http")
local note, check = http.note, http.check

local M = {}

M.attempts = 3
M.timeout = 60
M.highlight_limit = 800
M.content_limit = 20000

type Result = {
	title: string,
	url: string,
	highlights: { string },
	content: string?,
	date: string?,
	author: string?,
}

type SearchOptions = { count: number?, content: boolean? }

local function call(name: string, what: string, request: { [string]: unknown }): { [string]: any }
	if not host.endpoints[name] then
		error(what .. ": hx was started without EXA_API_KEY, so Exa cannot be used", 0)
	end
	return http.call({
		endpoint = name,
		body = json.encode(request),
		attempts = M.attempts,
		timeout = M.timeout,
		label = what:match("%.(.*)$") :: string,
		what = what,
		sender = "Exa",
		top_message = true,
	})
end

local function results_text(results: { Result }): string
	if #results == 0 then
		return "[no results]"
	end
	local blocks = {}
	for i, r in results do
		local about = {}
		for _, part in { r.author, r.date and r.date:sub(1, 10) } do
			if part and part ~= "" then
				table.insert(about, part)
			end
		end
		local lines = { ("%d. %s"):format(i, r.title) }
		if #about > 0 then
			lines[1] ..= " (" .. table.concat(about, ", ") .. ")"
		end
		table.insert(lines, "   " .. r.url)
		local text: string = http.clip(table.concat(r.highlights, " ... "), M.highlight_limit)
		if text ~= "" then
			table.insert(lines, "   " .. text)
		end
		if r.content then
			table.insert(lines, "   content:\n" .. r.content)
		end
		table.insert(blocks, table.concat(lines, "\n"))
	end
	return table.concat(blocks, "\n\n")
end
local Results = { __tostring = results_text }

local function text_of(value: unknown): string?
	return if type(value) == "string" and value ~= "" then value else nil
end

function M.search(query: string, options: SearchOptions?)
	check("web.search", query, 1, "query", "string")
	check("web.search", options, 2, "options", "table", true)
	local given: SearchOptions = options or {}
	if query:match("^%s*$") or #query > 2000 then
		error("web.search: the query must be 1 to 2000 bytes and not all spaces", 0)
	end
	local count = given.count or 10
	if type(count) ~= "number" or count < 1 or count > 50 or count % 1 ~= 0 then
		error("web.search: count must be a whole number from 1 to 50", 0)
	end
	note("  search " .. query:gsub("%c", " "))
	local answer = call("exa", "web.search", {
		query = query,
		type = "auto",
		numResults = count,
		contents = {
			highlights = true,
			text = if given.content then { maxCharacters = M.content_limit } else nil,
		},
	})
	local results: { Result } = {}
	for _, r in type(answer.results) == "table" and answer.results or {} do
		local highlights = {}
		for _, h in type(r.highlights) == "table" and r.highlights or {} do
			if type(h) == "string" then
				table.insert(highlights, (h:gsub("%s+", " ")))
			end
		end
		table.insert(results, {
			title = text_of(r.title) or "",
			url = text_of(r.url) or "",
			highlights = highlights,
			content = text_of(r.text),
			date = text_of(r.publishedDate),
			author = text_of(r.author),
		})
	end
	return setmetatable(results, Results)
end

function M.fetch(url: string): string
	check("web.fetch", url, 1, "url", "string")
	if not url:match("^[Hh][Tt][Tt][Pp][Ss]?://[^/%s]") or #url > 8192 then
		error("web.fetch: the url must start with http:// or https://, name a host, and be at most 8192 bytes", 0)
	end
	note("  fetch " .. url:gsub("%c", ""))
	local answer = call("contents", "web.fetch", { ids = { url }, text = true })
	local page = type(answer.results) == "table" and answer.results[1]
	if type(page) ~= "table" then
		local status = type(answer.statuses) == "table" and answer.statuses[1]
		local failure = type(status) == "table" and type(status.error) == "table" and status.error or {}
		error(("web.fetch: Exa could not read %s: %s"):format(url, text_of(failure.tag) or "it gave no reason"), 0)
	end
	local title = text_of(page.title) or ""
	local content = text_of(page.text) or ""
	if title ~= "" and not content:sub(1, 500):find(title, 1, true) then
		content = "# " .. title .. "\n\n" .. content
	end
	return content ~= "" and content or "[the page has no content]"
end

return M
