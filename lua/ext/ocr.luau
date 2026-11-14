-- reads the text of a PDF, Word, PowerPoint or image file, through DeepSeek's OCR: read(path, purpose?) -> text
--
-- Built into hx; a project's own .hx/ext/ocr.luau takes its place. The file goes to DeepSeek's OCR API with
-- DEEPSEEK_API_KEY, whatever model the loop talks to. hx adds the key; this code never sees it.

local M = {}

M.attempts = 3 -- a recognition that fails with 408, 429 or 5xx, or gets no answer, is tried this many times
M.timeout = 300 -- seconds per request: the upload, and up to 180 seconds of recognition

-- The purpose of a file whose caller named none. A PDF keeps its layout: tables, columns, figures and where they
-- are on the page. Anything else is read as plain text.
function M.purpose(path)
	return path:lower():match("%.pdf$") and "rich_layout_recognition" or "text_recognition"
end

local purposes = { text_recognition = true, rich_layout_recognition = true }

local function note(text)
	if not host.quiet then
		host.log(text)
	end
end

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

-- Sends the file and returns the decoded result. Retries what is worth retrying, then raises. A recognition that
-- fails after it started still answers 200, with an `error` in the body; only a busy or broken service is worth
-- another try then.
local function recognize(path, purpose)
	if not host.tools then
		error("ocr: hx was started without DEEPSEEK_API_KEY, so DeepSeek's OCR cannot be used", 0)
	end
	for attempt = 1, M.attempts do
		local status, text = host.tool("ocr", { file = path, purpose = purpose }, { timeout = M.timeout })
		local retry
		if status == 200 then
			local ok, value = pcall(json.decode, text) -- what comes before the JSON is keep-alive newlines
			if not ok or type(value) ~= "table" then
				error("ocr: DeepSeek sent an answer that is not JSON: " .. tostring(text):sub(1, 300), 0)
			end
			if not value.error then
				return value
			end
			local kind = type(value.error) == "table" and tostring(value.error.type or value.error.code or "") or ""
			retry = kind:find("unavailable") or kind:find("internal") or kind:find("server")
			status = "recognition failed"
		else
			retry = type(status) ~= "number" or status == 408 or status == 429 or status >= 500
		end
		if not retry or attempt == M.attempts then
			error(("ocr of %s failed (%s): %s"):format(path, tostring(status or "no answer"), message(text)), 0)
		end
		local wait = 2 ^ (attempt + 1)
		note(("  ocr failed (%s), retrying in %d s"):format(tostring(status or text), wait))
		host.sleep(wait)
	end
	error("unreachable")
end

-- read(path, purpose?) -> text
--
-- purpose is "text_recognition" or "rich_layout_recognition"; by default a PDF gets the second and anything else
-- the first. Up to 100 MiB and 1,000 pages. A document of more than one page comes back with a "[page N]" line
-- before each page. Page-by-page recognition can leave layout tokens with boxes (0-1000 of the page) in the text.
-- A page cut off by the output limit ends with a note that says so, and an empty page is marked as one.
function M.read(path, purpose)
	if type(path) ~= "string" then
		error("ocr.read: argument 1 (path) must be a string, not " .. type(path), 0)
	end
	purpose = purpose or M.purpose(path)
	if not purposes[purpose] then
		error('ocr.read: purpose must be "text_recognition" or "rich_layout_recognition"', 0)
	end
	note(("  ocr %s (%s)"):format(path:gsub("%c", ""), purpose))
	local result = recognize(path, purpose)
	local pages = type(result.pages) == "table" and result.pages or {}
	table.sort(pages, function(a, b)
		return (a.index or 0) < (b.index or 0)
	end)
	local parts = {}
	for _, page in pages do
		local text = type(page.text) == "string" and page.text or ""
		if text:match("^%s*$") then -- a blank page, or one the service came back empty on; another try may differ
			text = "[no text was recognized on this page]"
		end
		if #pages > 1 then
			text = ("[page %d]\n%s"):format((page.index or 0) + 1, text)
		end
		if page.finish_reason == "length" then
			text ..= "\n[this page was cut off: the recognition reached its output limit]"
		end
		table.insert(parts, text)
	end
	return #parts > 0 and table.concat(parts, "\n\n") or "[no text was recognized]"
end

return M
