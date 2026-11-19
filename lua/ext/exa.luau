-- researches a question on the web with Exa's Deep Search: deep(query, objective, {queries?, schema?, count?}) -> answer and sources
--!strict

local http = ext("_http")
local note = http.note

local M = {}

M.attempts = 3
M.timeout = 180
M.highlight_limit = 600
M.answer = "A grounded answer to the query that serves the objective: the specific facts and figures, "
	.. "with what disagrees between sources said plainly"

type Source = { title: string, url: string, date: string?, author: string?, highlights: { string } }

type Research = {
	answer: unknown,
	grounding: { any },
	results: { Source },
	notices: { string },
	cost: number?,
}

type DeepOptions = { queries: { string }?, schema: { [string]: any }?, count: number? }

local function check(value: unknown, position: number, name: string, kind: string, optional: boolean?)
	http.check("exa.deep", value, position, name, kind, optional)
end

local function call(request: { [string]: unknown }): { [string]: any }
	if not host.endpoints.exa then
		error("exa: hx was started without EXA_API_KEY, so Exa's Deep Search cannot be used", 0)
	end
	return http.call({
		endpoint = "exa",
		body = json.encode(request),
		attempts = M.attempts,
		timeout = M.timeout,
		label = "exa",
		what = "exa.deep",
		sender = "Exa",
		top_message = true,
	})
end

local function research_text(r: Research): string
	local parts: { string } = {}
	local answer = r.answer
	if type(answer) == "string" then
		if answer ~= "" then
			table.insert(parts, answer)
		end
	elseif answer ~= nil then
		local text: string = json.encode(answer)
		table.insert(parts, text)
	end
	for _, n in r.notices do
		table.insert(parts, "notice: " .. n)
	end
	if #r.results == 0 then
		table.insert(parts, "[no sources]")
	else
		local blocks = {}
		for i, s in r.results do
			local about = {}
			for _, part in { s.author, s.date and s.date:sub(1, 10) } do
				if part and part ~= "" then
					table.insert(about, part)
				end
			end
			local lines = { ("%d. %s"):format(i, s.title) }
			if #about > 0 then
				lines[1] ..= " (" .. table.concat(about, ", ") .. ")"
			end
			table.insert(lines, "   " .. s.url)
			local text: string = http.clip(table.concat(s.highlights, " ... "), M.highlight_limit)
			if text ~= "" then
				table.insert(lines, "   " .. text)
			end
			table.insert(blocks, table.concat(lines, "\n"))
		end
		table.insert(parts, "Sources:\n" .. table.concat(blocks, "\n\n"))
	end
	return table.concat(parts, "\n\n")
end
local ResearchText = { __tostring = research_text }

function M.deep(query: string, objective: string, options: DeepOptions?)
	check(query, 1, "query", "string")
	check(objective, 2, "objective", "string")
	check(options, 3, "options", "table", true)
	local given: DeepOptions = options or {}
	if query:match("^%s*$") then
		error("exa.deep: the query is empty", 0)
	end
	if objective:match("^%s*$") or #objective > 4096 then
		error("exa.deep: the objective must be 1 to 4096 characters and not all spaces", 0)
	end
	local count = given.count or 10
	if type(count) ~= "number" or count < 1 or count > 100 or count % 1 ~= 0 then
		error("exa.deep: count must be a whole number from 1 to 100", 0)
	end
	local queries = given.queries
	if queries ~= nil then
		check(queries, 3, "options.queries", "table")
		if #queries > 10 then
			error("exa.deep: queries holds at most 10 searches", 0)
		end
		for i, q in queries do
			if type(q) ~= "string" or q:match("^%s*$") then
				error(("exa.deep: queries[%s] must be a search, not %s"):format(tostring(i), type(q)), 0)
			end
		end
		queries = #queries > 0 and queries or nil
	end
	local schema = given.schema
	if schema ~= nil then
		check(schema, 3, "options.schema", "table")
		if schema.type ~= "object" then
			error('exa.deep: a schema is of type "object"; leave it out for a text answer', 0)
		end
	end

	note("  exa deep " .. query:gsub("%c", " "))
	local answer = call({
		query = query,
		objective = objective,
		additionalQueries = queries,
		type = "deep",
		numResults = count,
		contents = { highlights = true },
		outputSchema = schema or { type = "text", description = M.answer },
	})

	local results: { Source } = {}
	for _, r in type(answer.results) == "table" and answer.results or {} do
		local highlights = {}
		for _, h in type(r.highlights) == "table" and r.highlights or {} do
			if type(h) == "string" then
				table.insert(highlights, h)
			end
		end
		table.insert(results, {
			title = type(r.title) == "string" and r.title or "",
			url = type(r.url) == "string" and r.url or "",
			date = type(r.publishedDate) == "string" and r.publishedDate or nil,
			author = type(r.author) == "string" and r.author or nil,
			highlights = highlights,
		})
	end
	local notices: { string } = {}
	for _, n in type(answer.notices) == "table" and answer.notices or {} do
		if type(n) == "table" and type(n.message) == "string" then
			table.insert(notices, n.message)
		end
	end
	for _, w in type(answer.warnings) == "table" and answer.warnings or {} do
		if type(w) == "table" and type(w.message) == "string" then
			note("  exa warning: " .. w.message)
		end
	end
	local output = type(answer.output) == "table" and answer.output or {}
	local cost = type(answer.costDollars) == "table" and answer.costDollars.total or nil
	local research: Research = {
		answer = output.content ~= json.null and output.content or nil,
		grounding = type(output.grounding) == "table" and output.grounding or {},
		results = results,
		notices = notices,
		cost = type(cost) == "number" and cost or nil,
	}
	return setmetatable(research, ResearchText)
end

return M
