-- reads the text of a PDF, Word, PowerPoint or image file, through DeepSeek's OCR: read(path, purpose?) -> text
--!strict
--
-- Built into hx; a project's own .hx/ext/ocr.luau takes its place. The file goes to DeepSeek's OCR API with
-- DEEPSEEK_API_KEY, whatever model the loop talks to. hx adds the key; this code never sees it.

local http = ext("_http")
local note = http.note

local M = {}

M.attempts = 3 -- a recognition that fails with 408, 429 or 5xx, or gets no answer, is tried this many times
M.timeout = 300 -- seconds per request: the upload, and up to 180 seconds of recognition

-- The purpose of a file whose caller named none. A PDF keeps its layout: tables, columns, figures and where they
-- are on the page. Anything else is read as plain text.
function M.purpose(path: string): string
	return path:lower():match("%.pdf$") and "rich_layout_recognition" or "text_recognition"
end

-- A page of the result, as DeepSeek describes it; the code still checks what it reads.
type Page = { index: number?, text: string?, finish_reason: string? }

local purposes = { text_recognition = true, rich_layout_recognition = true }

-- Sends the file and returns the decoded result. Retries what is worth retrying, then raises. A recognition that
-- fails after it started still answers 200, with an `error` in the body; only a busy or broken service is worth
-- another try then. What comes before the JSON of a 200 is keep-alive newlines, which json.decode skips.
local function recognize(path: string, purpose: string): { [string]: any }
	if not host.tools then
		error("ocr: hx was started without DEEPSEEK_API_KEY, so DeepSeek's OCR cannot be used", 0)
	end
	return http.call({
		endpoint = "ocr",
		body = { file = path, purpose = purpose },
		attempts = M.attempts,
		timeout = M.timeout,
		label = "ocr",
		what = "ocr of " .. path,
		sender = "DeepSeek",
		first_wait = 4, -- then 8: ocr waits twice as long as web and exa
		body_error = {
			status = "recognition failed",
			retry = function(err: any): boolean
				local kind = if type(err) == "table" then tostring(err.type or err.code or "") else ""
				return (kind:find("unavailable") or kind:find("internal") or kind:find("server")) ~= nil
			end,
		},
	})
end

-- read(path, purpose?) -> text
--
-- purpose is "text_recognition" or "rich_layout_recognition"; by default a PDF gets the second and anything else
-- the first. Up to 100 MiB and 1,000 pages. A document of more than one page comes back with a "[page N]" line
-- before each page. Page-by-page recognition can leave layout tokens with boxes (0-1000 of the page) in the text.
-- A page cut off by the output limit ends with a note that says so, and an empty page is marked as one.
function M.read(path: string, purpose: string?): string
	http.check("ocr.read", path, 1, "path", "string")
	local chosen = purpose or M.purpose(path)
	if not purposes[chosen] then
		error('ocr.read: purpose must be "text_recognition" or "rich_layout_recognition"', 0)
	end
	note(("  ocr %s (%s)"):format(path:gsub("%c", ""), chosen))
	local result = recognize(path, chosen)
	local pages: { Page } = type(result.pages) == "table" and result.pages or {}
	table.sort(pages, function(a: Page, b: Page): boolean
		return (a.index or 0) < (b.index or 0)
	end)
	local parts = {}
	for _, page in pages do
		local text: string = type(page.text) == "string" and page.text or ""
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
