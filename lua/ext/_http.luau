-- what the built-in extensions that call a web API share: progress lines, error messages, retries, argument checks
--!strict

local M = {}

function M.note(text: string)
	if not host.quiet then
		host.log(text)
	end
end

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

function M.check(fname: string, value: unknown, position: number, name: string, kind: string, optional: boolean?)
	if type(value) == kind or value == nil and optional then
		return
	end
	error(("%s: argument %d (%s) must be a %s, not %s"):format(fname, position, name, kind, type(value)), 0)
end

function M.clip(text: string, limit: number): string
	text = text:gsub("%s+", " ")
	local length = utf8.len(text)
	if length and length > limit then
		text = text:sub(1, utf8.offset(text, limit + 1) - 1) .. " ..."
	elseif not length and #text > limit then
		text = text:sub(1, limit) .. " ..."
	end
	return text
end

type Request = {
	endpoint: string,
	body: string | { [string]: string },
	attempts: number,
	timeout: number,
	label: string,
	what: string,
	sender: string,
	top_message: boolean?,
	first_wait: number?,
	body_error: { status: string?, retry: ((err: any) -> boolean)? }?,
}

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
