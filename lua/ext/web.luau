-- searches the web and fetches pages, through DeepSeek: search(query, {count?, content?, question?}) -> results, fetch(url) -> markdown
--!strict

local http = ext("_http")
local note, check = http.note, http.check

local M = {}

M.attempts = 3
M.timeout = 60
M.summary_limit = 800

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

local function call(name: string, request: { [string]: unknown }): { [string]: any }
	if not host.endpoints[name] then
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
		body_error = {},
	})
end

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
			question = nil
		elseif characters > 128 then
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
