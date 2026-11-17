-- HTTP requests through curl, as data: request(url, {method?, headers?, body?, json?, timeout?}) -> {status, headers, body}; any status is returned, only a failed connection raises
--!strict

local M = {}

type Options = {
	method: string?,
	headers: { [string]: string }?,
	body: string?,
	json: any,
	timeout: number?,
}

local function check(value: unknown, name: string, kind: string, required: boolean?)
	if (value ~= nil or required) and type(value) ~= kind then
		error(("http.request: %s must be a %s, not %s"):format(name, kind, type(value)), 0)
	end
end

local function last_block(text: string): (number, { [string]: string })
	local status, headers = 0, {}
	for line in (text .. "\n"):gmatch("([^\n]*)\n") do
		line = line:gsub("\r$", "")
		local code = line:match("^HTTP/[%d.]+ (%d%d%d)")
		if code then
			status, headers = tonumber(code) :: number, {}
		else
			local name, value = line:match("^([^:%s]+):%s*(.-)%s*$")
			if name and value then
				local key = name:lower()
				headers[key] = if headers[key] then headers[key] .. ", " .. value else value
			end
		end
	end
	return status, headers
end

function M.request(url: string, options: Options?): { status: number, headers: { [string]: string }, body: string }
	check(url, "url", "string", true)
	check(options, "options", "table")
	local opts: Options = options or {}
	check(opts.method, "method", "string")
	check(opts.headers, "headers", "table")
	check(opts.body, "body", "string")
	check(opts.timeout, "timeout", "number")
	if opts.body ~= nil and opts.json ~= nil then
		error("http.request: give body or json, not both", 0)
	end

	local made = host.sh({ "mktemp", "-d" })
	if made.code ~= 0 then
		error("http.request: cannot make a directory for the response: " .. made.out, 0)
	end
	local where = (made.out:gsub("%s+$", ""))
	local head_file, body_file = where .. "/head", where .. "/body"
	local timeout = opts.timeout or 60
	local command: { string | number } = { "curl", "-sS", "-L", "--max-time", timeout, "-D", head_file, "-o", body_file }
	local method = opts.method and opts.method:upper()
	if method == "HEAD" then
		table.insert(command, "-I")
	elseif method then
		table.insert(command, "-X")
		table.insert(command, method)
	end
	local body = if opts.json ~= nil then json.encode(opts.json) else opts.body
	local headers: { [string]: string } = table.clone(opts.headers or {})
	if opts.json ~= nil and not headers["Content-Type"] and not headers["content-type"] then
		headers["Content-Type"] = "application/json"
	end
	for name, value in headers do
		table.insert(command, "-H")
		table.insert(command, ("%s: %s"):format(name, value))
	end
	if body then
		table.insert(command, "--data-binary")
		table.insert(command, "@-")
	end
	table.insert(command, "--")
	table.insert(command, url)

	local result = host.sh(command, { input = body, timeout = timeout + 10 })
	local head: string? = host.read(head_file)
	local data: string? = if method == "HEAD" then "" else host.read(body_file)
	host.sh({ "rm", "-rf", where })
	if result.code ~= 0 then
		error(("http.request %s: %s"):format(url, (tostring(result.out):gsub("%s+$", ""))), 0)
	end
	local status, response = last_block(head or "")
	return { status = status, headers = response, body = data or "" }
end

return M
