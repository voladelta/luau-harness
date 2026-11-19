-- reads the text of a PDF, Word, PowerPoint or image file, through DeepSeek's OCR: read(path, purpose?) -> text
--!strict

local http = ext("_http")
local note = http.note

local M = {}

M.attempts = 3
M.timeout = 300

function M.purpose(path: string): string
	return path:lower():match("%.pdf$") and "rich_layout_recognition" or "text_recognition"
end

type Page = { index: number?, text: string?, finish_reason: string? }

local purposes = { text_recognition = true, rich_layout_recognition = true }

local function recognize(path: string, purpose: string): { [string]: any }
	if not host.endpoints.ocr then
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
		first_wait = 4,
		body_error = {
			status = "recognition failed",
			retry = function(err: any): boolean
				local kind = if type(err) == "table" then tostring(err.type or err.code or "") else ""
				return (kind:find("unavailable") or kind:find("internal") or kind:find("server")) ~= nil
			end,
		},
	})
end

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
		if text:match("^%s*$") then
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
