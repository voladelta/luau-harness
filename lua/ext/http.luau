-- HTTP requests through curl, as data: request(url, {method?, headers?, body?, json?, timeout?}) -> {status, headers, body}; any status is returned, only a failed connection raises
--!strict

local M = {}

local largest_body = 64 * 1024 * 1024
local too_large = "larger than 64 MiB"
local seq = 0
local stale: { string } = {}

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

local function temp_dir(): string
	local dir = host.env("TMPDIR")
	if not dir or dir == "" then
		return "/tmp"
	end
	local trimmed = dir:gsub("/+$", "")
	return if trimmed == "" then "/" else trimmed
end

local function without_path(err: string?, path: string): string
	local text = tostring(err)
	return if text:sub(1, #path + 2) == path .. ": " then text:sub(#path + 3) else text
end

local function last_block(text: string): (number, { [string]: string })
	local status: number, headers: { [string]: string } = 0, {}
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

local function is_token(text: string): boolean
	return text:match("^[0-9A-Za-z!#$%%&'*+%-.%^_`|~]+$") ~= nil
end

local function sweep_stale()
	while #stale > 0 do
		local count = math.min(#stale, 200)
		local cmd: { string | number } = { "rm", "-f", "--" }
		table.move(stale, 1, count, 4, cmd)
		local rm_ok, rm_result = pcall(host.sh, cmd)
		if not rm_ok or type(rm_result) ~= "table" or rm_result.code ~= 0 then
			return
		end
		if count == #stale then
			table.clear(stale)
		else
			table.move(stale, count + 1, #stale, 1, stale)
			for i = #stale, #stale - count + 1, -1 do
				stale[i] = nil
			end
		end
	end
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

	local timeout = opts.timeout or 60
	if not (timeout > 0 and timeout < math.huge) then
		error("http.request: timeout must be a positive finite number", 0)
	end
	local method = opts.method and opts.method:upper()
	if method ~= nil and not is_token(method) then
		error(("http.request: invalid method %q"):format(opts.method :: string), 0)
	end
	local body = if opts.json ~= nil then json.encode(opts.json) else opts.body
	local headers: { [string]: string } = table.clone(opts.headers or {})
	local has_content_type = false
	for name, value in headers do
		check(name, "header name", "string", true)
		check(value, ("header %q value"):format(name), "string", true)
		if not is_token(name) then
			error(("http.request: invalid header name %q"):format(name), 0)
		end
		if value:find("[\r\n]") or value:find("\0", 1, true) then
			error(("http.request: invalid header value for %q"):format(name), 0)
		end
		if name:lower() == "content-type" then
			has_content_type = true
		end
	end
	if opts.json ~= nil and not has_content_type then
		headers["Content-Type"] = "application/json"
	end

	seq += 1
	local sec, nsec = host.wall_time()
	local prefix = ("%s/hx-http-%d-%d-%d-%x"):format(temp_dir(), sec, nsec, seq, math.random(0, 0x7fffffff))
	local head_file, body_file = prefix .. ".head", prefix .. ".body"
	table.insert(stale, head_file)
	table.insert(stale, body_file)
	local command: { string | number } = {
		"curl",
		"-sS",
		"-g",
		"-L",
		"--proto",
		"=http,https",
		"--proto-redir",
		"=http,https",
		"--max-time",
		timeout,
		"--max-filesize",
		largest_body,
		"-D",
		head_file,
		"-o",
		body_file,
	}
	if method == "HEAD" then
		table.insert(command, "-I")
	elseif method then
		table.insert(command, "-X")
		table.insert(command, method)
	end
	for name, value in headers do
		table.insert(command, "-H")
		table.insert(command, if value == "" then name .. ";" else ("%s: %s"):format(name, value))
	end
	if body then
		table.insert(command, "--data-binary")
		table.insert(command, "@-")
	end
	table.insert(command, "--")
	table.insert(command, url)

	local ok, result, head, head_err, data, data_err = pcall(function()
		local ran = host.sh(command, { input = body, timeout = timeout + 10 })
		if ran.code ~= 0 then
			return ran, nil, nil, nil, nil
		end
		local h: string?, herr: string? = nil, nil
		if host.stat(head_file) then
			h, herr = host.read(head_file)
		else
			h = ""
		end
		local d: string?, derr: string? = nil, nil
		if method == "HEAD" or not host.stat(body_file) then
			d = ""
		else
			d, derr = host.read(body_file)
		end
		return ran, h, herr, d, derr
	end)
	sweep_stale()
	if not ok then
		error(result, 0)
	end
	if result.code == 63 then
		error(("http.request %s: the response body: %s"):format(url, too_large), 0)
	end
	if result.code ~= 0 then
		local detail = (tostring(result.out or ""):gsub("%s+$", ""))
		if detail == "" then
			detail = if result.timed_out
				then ("timed out after %g s"):format(timeout)
				elseif result.signal then ("killed by signal %d"):format(result.signal)
				else ("curl exited with status %s"):format(tostring(result.code))
		end
		error(("http.request %s: %s"):format(url, detail), 0)
	end
	if not head then
		error(("http.request %s: the response headers: %s"):format(url, without_path(head_err, head_file)), 0)
	end
	if not data then
		error(("http.request %s: the response body: %s"):format(url, without_path(data_err, body_file)), 0)
	end
	local status, response = last_block(head)
	return { status = status, headers = response, body = data }
end

return M
