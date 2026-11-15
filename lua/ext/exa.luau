-- researches a question on the web with Exa's Deep Search: deep(query, objective, {queries?, schema?, count?}) -> answer and sources
--!strict
--
-- Built into hx; a project's own .hx/ext/exa.luau takes its place. The call goes to Exa's /search endpoint with
-- type "deep" and EXA_API_KEY, whatever model the loop talks to. hx adds the key; this code never sees it.
--
-- Deep Search plans several searches, reads what they find, searches again where the evidence is thin or
-- contradicts itself, and writes a grounded answer from the pages it kept. Use it when finding the answer takes
-- research: comparisons, lists of items that each need their own search, multi-hop questions. For one
-- well-formed lookup, ext("web").search is quicker and cheaper.

local http = ext("_http")
local note = http.note

local M = {}

M.attempts = 3 -- a request that fails with 408, 429 or 5xx, or gets no answer, is tried this many times
M.timeout = 180 -- seconds per request; deep takes 4 to 15 seconds and synthesis a few more, but can run longer
M.highlight_limit = 600 -- characters of each result's highlights shown when printed; the field keeps all of them
M.answer = "A grounded answer to the query that serves the objective: the specific facts and figures, "
	.. "with what disagrees between sources said plainly"

-- One source of the research, as deep returns it.
type Source = { title: string, url: string, date: string?, author: string?, highlights: { string } }

-- What deep returns. The answer is text, or a table shaped by the schema; grounding is Exa's, as it sent it.
type Research = {
	answer: unknown,
	grounding: { any },
	results: { Source },
	notices: { string },
	cost: number?,
}

type DeepOptions = { queries: { string }?, schema: { [string]: any }?, count: number? }

-- Raises when an argument of deep is not of its kind.
local function check(value: unknown, position: number, name: string, kind: string, optional: boolean?)
	http.check("exa.deep", value, position, name, kind, optional)
end

-- Posts the request to Exa's /search and returns the decoded answer. Retries what is worth retrying, then raises.
local function call(request: { [string]: unknown }): { [string]: any }
	if not host.exa then
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

-- What printing a research result shows: the answer, the notices, then the sources, best first.
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

-- deep(query, objective, {queries = nil, schema = nil, count = 10}) -> research
--
-- query says what to research, in plain words: the subject, and any source type or period that changes what a
-- good page looks like. objective is the goal this search serves: which documents should rank first, which should
-- be left out, and what specific facts or figures to pull from them. queries is a list of up to 10 further
-- searches to start from, each a different direction, not a rephrasing. schema is an Exa output schema of type
-- "object" (at most 10 properties in all, 2 levels deep, every array with items), for an answer as a table; by
-- default the answer is text. count is how many sources come back, 1 to 100 (the price covers 10).
--
-- The research is {answer (text, or a table for a schema), grounding ({field, citations = {{url, title}},
-- confidence}), results ({title, url, date?, author?, highlights}), notices (strings), cost (dollars or nil)}.
-- Printed or returned from lua, it reads as the answer and a numbered list of sources.
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
	-- Warnings describe the request, for whoever maintains this code, not for the model.
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
