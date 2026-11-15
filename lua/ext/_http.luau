-- what the built-in extensions that call a web API share: progress lines, error messages, retries, argument checks
--!strict
--
-- Built into hx, for web, exa and ocr, which load it with ext("_http"). A name that starts with _ marks an
-- extension that is there for other extensions, not for the model. The --!strict line comes second because the
-- first line is what the system prompt shows.

local M = {}

-- A progress line on stderr, unless hx runs with --quiet.
function M.note(text: string)
	if not host.quiet then
		host.log(text)
	end
end

-- The message of an error body, {"error": "..."} or {"error": {"message": ...}}, or the start of the text. Exa
-- may also put its message at the top, {"message": ...}, which `top` takes too.
function M.message(text: string?, top: boolean?): string
	local ok, body = pcall(json.decode, text or "")
	if ok and type(body) == "table" then
		local err = body.error
		if type(err) == "table" and type(err.message) == "string" then
			return err.message
		elseif type(err) == "string" then
			return err
		elseif top and type(body.message) == "string" then
			return body.message
		end
	end
	return tostring(text):sub(1, 300)
end

-- Raises when an argument is not of its kind: "web.search: argument 1 (query) must be a string, not table".
function M.check(fname: string, value: unknown, position: number, name: string, kind: string, optional: boolean?)
	if type(value) == kind or value == nil and optional then
		return
	end
	error(("%s: argument %d (%s) must be a %s, not %s"):format(fname, position, name, kind, type(value)), 0)
end

-- The text with its runs of white space made one space, cut to `limit` characters with " ..." when it has more
-- than `limit` bytes. The cut falls on a character boundary, so a cut text is still UTF-8.
function M.clip(text: string, limit: number): string
	text = text:gsub("%s+", " ")
	if #text > limit then
		local cut = utf8.offset(text, limit + 1) or #text + 1
		text = text:sub(1, cut - 1) .. " ..."
	end
	return text
end

type Request = {
	endpoint: string, -- host.tool's name for the API
	body: string | { [string]: string }, -- what host.tool sends: JSON text, or a form
	attempts: number,
	timeout: number,
	label: string, -- names the call in a progress line and in the error for an answer that is not JSON
	what: string, -- what failed, in the error raised when the request does
	sender: string, -- who answered: "DeepSeek" or "Exa"
	top_message: boolean?, -- the error body may carry its message at the top (see message)
	first_wait: number?, -- seconds before the second try, doubled for each try after it; 2 by default
	-- A 200 whose body has an `error` is an answer like any other when this is nil. Otherwise the request failed.
	-- Without a `status`, the error raised says only that, and there is no other try; with one, the error names
	-- it, and the request is tried again when `retry` says so.
	body_error: { status: string?, retry: ((err: any) -> boolean)? }?,
}

-- Sends the request and returns the decoded answer. A request is worth another try when it got no answer or 408,
-- 429 or 5xx; after the last try, or a failure that another try would not change, it raises.
function M.call(request: Request): { [string]: any }
	for attempt = 1, request.attempts do
		local status: (number | string)?, text: string? = host.tool(request.endpoint, request.body, {
			timeout = request.timeout,
		})
		local retry
		if status == 200 then
			local ok, value = pcall(json.decode, text)
			if not ok or type(value) ~= "table" then
				error(("%s: %s sent an answer that is not JSON: %s"):format(request.label, request.sender,
					tostring(text):sub(1, 300)), 0)
			end
			local body_error = request.body_error
			if not value.error or not body_error then
				return value
			end
			if not body_error.status then
				error(("%s failed: %s"):format(request.what, M.message(text, request.top_message)), 0)
			end
			retry = body_error.retry ~= nil and body_error.retry(value.error)
			status = body_error.status
		else
			retry = type(status) ~= "number" or status == 408 or status == 429 or status >= 500
		end
		if not retry or attempt == request.attempts then
			error(("%s failed (%s): %s"):format(request.what, tostring(status or "no answer"),
				M.message(text, request.top_message)), 0)
		end
		local wait = (request.first_wait or 2) * 2 ^ (attempt - 1)
		M.note(("  %s failed (%s), retrying in %d s"):format(request.label, tostring(status or text), wait))
		host.sleep(wait)
	end
	error("unreachable")
end

return M
