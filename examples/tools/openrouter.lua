-- OpenRouter API Tool for Luau Harness
-- Generates text, images, sounds, and videos via OpenRouter APIs.
-- Supports remaining credits inquiry, 24h cached help/llms.txt, and 24h cached model lists.
-- Refuses registration if OPENROUTER_API_KEY is missing from the environment.
--
-- Documentation:
-- - Models: https://openrouter.ai/docs/api/api-reference/models
-- - Help index: https://openrouter.ai/docs/llms.txt
-- - Model help: https://openrouter.ai/{provider}/{model-id}/llms.txt
-- - Credits: https://openrouter.ai/api/v1/credits
-- - Chat: https://openrouter.ai/api/v1/chat/completions
-- - Images: https://openrouter.ai/api/v1/images
-- - Audio: https://openrouter.ai/api/v1/audio/speech
-- - Videos: https://openrouter.ai/api/v1/videos

local b64_chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
local b64_lookup = {}
for i = 1, #b64_chars do
	b64_lookup[string.byte(b64_chars, i)] = i - 1
end
b64_lookup[string.byte("-")] = 62
b64_lookup[string.byte("_")] = 63

local function base64_decode(data)
	if type(data) ~= "string" or #data == 0 then return "" end
	data = data:gsub("%s+", "")
	local len = #data
	local out = {}
	local byte_buf = {}
	local i = 1
	while i <= len do
		local c1 = b64_lookup[string.byte(data, i)]
		local c2 = b64_lookup[string.byte(data, i + 1)]
		local b3 = string.byte(data, i + 2)
		local b4 = string.byte(data, i + 3)
		local c3 = b3 and b64_lookup[b3]
		local c4 = b4 and b64_lookup[b4]
		if not c1 or not c2 then break end

		byte_buf[#byte_buf + 1] = bit32.bor(bit32.lshift(c1, 2), bit32.rshift(c2, 4))
		if c3 then
			byte_buf[#byte_buf + 1] = bit32.bor(bit32.lshift(bit32.band(c2, 0x0f), 4), bit32.rshift(c3, 2))
			if c4 then
				byte_buf[#byte_buf + 1] = bit32.bor(bit32.lshift(bit32.band(c3, 0x03), 6), c4)
			end
		end

		if #byte_buf >= 2046 then
			out[#out + 1] = string.char(table.unpack(byte_buf))
			byte_buf = {}
		end
		i = i + 4
	end
	if #byte_buf > 0 then
		out[#out + 1] = string.char(table.unpack(byte_buf))
	end
	return table.concat(out)
end

local json = {}

function json.parse(str)
	local pos = 1
	local len = #str
	local function skip_whitespace()
		while pos <= len do
			local b = string.byte(str, pos)
			if b == 32 or b == 9 or b == 10 or b == 13 then
				pos = pos + 1
			else
				break
			end
		end
	end

	local parse_val
	local function parse_str()
		pos = pos + 1
		local chunks = {}
		local chunk_start = pos
		while pos <= len do
			local b = string.byte(str, pos)
			if b == 34 then
				if pos > chunk_start then
					chunks[#chunks + 1] = str:sub(chunk_start, pos - 1)
				end
				pos = pos + 1
				return table.concat(chunks)
			elseif b == 92 then
				if pos > chunk_start then
					chunks[#chunks + 1] = str:sub(chunk_start, pos - 1)
				end
				local esc = string.byte(str, pos + 1)
				if not esc then error("unterminated escape in string") end
				if esc == 34 then
					chunks[#chunks + 1] = '"'
					pos = pos + 2
					chunk_start = pos
				elseif esc == 92 then
					chunks[#chunks + 1] = '\\'
					pos = pos + 2
					chunk_start = pos
				elseif esc == 47 then
					chunks[#chunks + 1] = '/'
					pos = pos + 2
					chunk_start = pos
				elseif esc == 98 then
					chunks[#chunks + 1] = '\b'
					pos = pos + 2
					chunk_start = pos
				elseif esc == 102 then
					chunks[#chunks + 1] = '\f'
					pos = pos + 2
					chunk_start = pos
				elseif esc == 110 then
					chunks[#chunks + 1] = '\n'
					pos = pos + 2
					chunk_start = pos
				elseif esc == 114 then
					chunks[#chunks + 1] = '\r'
					pos = pos + 2
					chunk_start = pos
				elseif esc == 116 then
					chunks[#chunks + 1] = '\t'
					pos = pos + 2
					chunk_start = pos
				elseif esc == 117 then
					local hex = str:sub(pos + 2, pos + 5)
					if #hex ~= 4 or not hex:match("^%x%x%x%x$") then
						error("invalid unicode escape")
					end
					local cp = tonumber(hex, 16)
					pos = pos + 6
					if cp >= 0xD800 and cp <= 0xDBFF then
						local low_hex = str:sub(pos + 2, pos + 5)
						local low = low_hex:match("^%x%x%x%x$") and tonumber(low_hex, 16)
						if str:sub(pos, pos + 1) ~= "\\u" or not low or low < 0xDC00 or low > 0xDFFF then
							error("invalid unicode surrogate pair")
						end
						cp = 0x10000 + (cp - 0xD800) * 0x400 + low - 0xDC00
						pos = pos + 6
					elseif cp >= 0xDC00 and cp <= 0xDFFF then
						error("unpaired unicode surrogate")
					end
					chunks[#chunks + 1] = utf8.char(cp)
					chunk_start = pos
				else
					error("invalid string escape")
				end
			else
				pos = pos + 1
			end
		end
		error("unterminated string")
	end

	local function parse_num()
		local s = pos
		while pos <= len do
			local b = string.byte(str, pos)
			if (b >= 48 and b <= 57) or b == 43 or b == 45 or b == 46 or b == 101 or b == 69 then
				pos = pos + 1
			else
				break
			end
		end
		local n = tonumber(str:sub(s, pos - 1))
		if not n then error("invalid number") end
		return n
	end

	local function parse_arr()
		pos = pos + 1
		local arr = {}
		skip_whitespace()
		if string.byte(str, pos) == 93 then
			pos = pos + 1
			return arr
		end
		while pos <= len do
			arr[#arr + 1] = parse_val()
			skip_whitespace()
			local b = string.byte(str, pos)
			if b == 93 then
				pos = pos + 1
				return arr
			elseif b == 44 then
				pos = pos + 1
			else
				error("expected , or ] in array")
			end
		end
		error("unterminated array")
	end

	local function parse_obj()
		pos = pos + 1
		local obj = {}
		skip_whitespace()
		if string.byte(str, pos) == 125 then
			pos = pos + 1
			return obj
		end
		while pos <= len do
			skip_whitespace()
			if string.byte(str, pos) ~= 34 then error("expected string key in object") end
			local key = parse_str()
			skip_whitespace()
			if string.byte(str, pos) ~= 58 then error("expected : in object") end
			pos = pos + 1
			obj[key] = parse_val()
			skip_whitespace()
			local b = string.byte(str, pos)
			if b == 125 then
				pos = pos + 1
				return obj
			elseif b == 44 then
				pos = pos + 1
			else
				error("expected , or } in object")
			end
		end
		error("unterminated object")
	end

	parse_val = function()
		skip_whitespace()
		if pos > len then error("unexpected EOF") end
		local b = string.byte(str, pos)
		if b == 34 then
			return parse_str()
		elseif b == 123 then
			return parse_obj()
		elseif b == 91 then
			return parse_arr()
		elseif (b >= 48 and b <= 57) or b == 45 then
			return parse_num()
		elseif str:sub(pos, pos + 3) == "true" then
			pos = pos + 4
			return true
		elseif str:sub(pos, pos + 4) == "false" then
			pos = pos + 5
			return false
		elseif str:sub(pos, pos + 3) == "null" then
			pos = pos + 4
			return nil
		else
			error("unexpected character: " .. str:sub(pos, pos))
		end
	end

	local value = parse_val()
	skip_whitespace()
	if pos <= len then error("unexpected trailing JSON content") end
	return value
end

local function quote_json(value)
	return '"' .. value:gsub('[%z\1-\31\\"]', function(char)
		if char == '"' then return '\\"' end
		if char == '\\' then return '\\\\' end
		return string.format("\\u%04x", string.byte(char))
	end) .. '"'
end

function json.encode(val)
	local t = type(val)
	if t == "nil" then
		return "null"
	elseif t == "boolean" then
		return tostring(val)
	elseif t == "number" then
		return tostring(val)
	elseif t == "string" then
		return quote_json(val)
	elseif t == "table" then
		local len = #val
		if len > 0 then
			local parts = {}
			for i = 1, len do
				parts[#parts + 1] = json.encode(val[i])
			end
			return "[" .. table.concat(parts, ",") .. "]"
		else
			local has_keys = false
			local parts = {}
			for k, v in pairs(val) do
				has_keys = true
				parts[#parts + 1] = quote_json(tostring(k)) .. ":" .. json.encode(v)
			end
			if not has_keys then
				return "[]"
			end
			return "{" .. table.concat(parts, ",") .. "}"
		end
	end
	return "null"
end

local function read_cache(path, force)
	if force then return nil end
	local ok, raw = pcall(read, path)
	if not ok or type(raw) ~= "string" or #raw == 0 then
		return nil
	end
	local nl = raw:find("\n")
	if not nl then return nil end
	local ts = tonumber(raw:sub(1, nl - 1))
	if not ts then return nil end
	if (os.time() - ts) < 86400 then
		return raw:sub(nl + 1)
	end
	return nil
end

local function write_cache(path, content)
	local payload = tostring(os.time()) .. "\n" .. content
	pcall(write, path, payload)
end

return {
	capabilities = {
		fs = { "read", "write" },
	},
	http = {
		credits = {
			url = "https://openrouter.ai/api/v1/credits",
			method = "GET",
			secret = "OPENROUTER_API_KEY",
			bearer = true,
			read_only = true,
		},
		models = {
			url = "https://openrouter.ai/api/v1/models",
			method = "GET",
			secret = "OPENROUTER_API_KEY",
			bearer = true,
			read_only = true,
		},
		model_help = {
			url = "https://openrouter.ai/{provider}/{model_id}/llms.txt",
			method = "GET",
			secret = "OPENROUTER_API_KEY",
			bearer = true,
			raw = true,
			read_only = true,
		},
		docs_index = {
			url = "https://openrouter.ai/docs/llms.txt",
			method = "GET",
			secret = "OPENROUTER_API_KEY",
			bearer = true,
			raw = true,
			read_only = true,
		},
		chat = {
			url = "https://openrouter.ai/api/v1/chat/completions",
			method = "POST",
			secret = "OPENROUTER_API_KEY",
			bearer = true,
			read_only = false,
		},
		images = {
			url = "https://openrouter.ai/api/v1/images",
			method = "POST",
			secret = "OPENROUTER_API_KEY",
			bearer = true,
			read_only = false,
		},
		speech = {
			url = "https://openrouter.ai/api/v1/audio/speech",
			method = "POST",
			secret = "OPENROUTER_API_KEY",
			bearer = true,
			raw = true,
			read_only = false,
		},
		videos = {
			url = "https://openrouter.ai/api/v1/videos",
			method = "POST",
			secret = "OPENROUTER_API_KEY",
			bearer = true,
			read_only = false,
		},
		poll_video = {
			url = "https://openrouter.ai/api/v1/videos/{job_id}",
			method = "GET",
			secret = "OPENROUTER_API_KEY",
			bearer = true,
			read_only = true,
		},
	},
	tools = {
		credits = {
			signature = "credits(args: {force: boolean?}?): table",
			description = "Check total credits purchased, total usage, and remaining balance in USD for the authenticated OpenRouter user.",
			run = function(args)
				local res = http("credits", {})
				local data = res and res.data or {}
				local total = data.total_credits or 0
				local usage = data.total_usage or 0
				local remaining = total - usage
				return {
					total_credits = total,
					total_usage = usage,
					remaining_credits = remaining,
				}
			end,
		},
		models = {
			signature = "models(args: {output_modalities: string?, supported_parameters: string?, sort: string?, force: boolean?, search: string?}?): table",
			description = "Browse and search OpenRouter models, cached locally for 24 hours (unless force=true). Supports filtering by output_modalities ('text', 'image', 'video', 'audio', 'speech', 'all'), supported_parameters ('tools'), sort ('pricing-low-to-high', 'pricing-high-to-low', 'context-high-to-low', 'newest'), or search substring.",
			run = function(args)
				args = args or {}
				local force = args.force == true
				local cache_path = ".openrouter/models.json"
				local cached_str = read_cache(cache_path, force)
				local model_list

				if cached_str then
					local ok, parsed = pcall(json.parse, cached_str)
					if ok and type(parsed) == "table" and type(parsed.data) == "table" then
						model_list = parsed
					end
				end

				if not model_list then
					local query = { output_modalities = "all" }
					local res = http("models", query)
					if res and type(res.data) == "table" then
						model_list = res
						local encoded = json.encode(res)
						write_cache(cache_path, encoded)
					else
						error("failed to fetch models from OpenRouter")
					end
				end

				local items = model_list.data or {}
				local filtered = {}
				local search = args.search and args.search:lower()
				local modality = args.output_modalities and args.output_modalities:lower()

				for _, m in ipairs(items) do
					local keep = true
					if search and #search > 0 then
						local id = (m.id or ""):lower()
						local name = (m.name or ""):lower()
						if not (id:find(search, 1, true) or name:find(search, 1, true)) then
							keep = false
						end
					end

					if keep and modality and modality ~= "all" then
						local arch = m.architecture
						local mods = arch and arch.output_modalities or {}
						local matched = false
						for _, mod in ipairs(mods) do
							if mod:lower() == modality then
								matched = true
								break
							end
						end
						if not matched then
							keep = false
						end
					end

					if keep and args.supported_parameters and #args.supported_parameters > 0 then
						local model_params = {}
						if type(m.supported_parameters) == "table" then
							for _, p in ipairs(m.supported_parameters) do
								model_params[p:lower()] = true
							end
						end
						for req_param in args.supported_parameters:gmatch("[^,]+") do
							local p_clean = req_param:match("^%s*(.-)%s*$"):lower()
							if #p_clean > 0 and not model_params[p_clean] then
								keep = false
								break
							end
						end
					end

					if keep then
						filtered[#filtered + 1] = m
					end
				end

				if args.sort and #filtered > 1 then
					local sort_mode = args.sort:lower()
					local function model_price(m)
						local p = m.pricing
						if not p then return nil end
						local prompt = tonumber(p.prompt)
						local comp = tonumber(p.completion)
						if prompt or comp then
							return (prompt or 0) + (comp or 0)
						end
						return nil
					end

					if sort_mode == "pricing-low-to-high" then
						table.sort(filtered, function(a, b)
							local pa = model_price(a)
							local pb = model_price(b)
							if pa and pb then return pa < pb end
							if pa and not pb then return true end
							return false
						end)
					elseif sort_mode == "pricing-high-to-low" then
						table.sort(filtered, function(a, b)
							local pa = model_price(a)
							local pb = model_price(b)
							if pa and pb then return pa > pb end
							if pa and not pb then return true end
							return false
						end)
					elseif sort_mode == "context-high-to-low" then
						table.sort(filtered, function(a, b)
							local ca = tonumber(a.context_length)
							local cb = tonumber(b.context_length)
							if ca and cb then return ca > cb end
							if ca and not cb then return true end
							return false
						end)
					elseif sort_mode == "newest" then
						table.sort(filtered, function(a, b)
							local na = tonumber(a.created)
							local nb = tonumber(b.created)
							if na and nb then return na > nb end
							if na and not nb then return true end
							return false
						end)
					end
				end

				return {
					data = filtered,
					total_count = #filtered,
					cached = (cached_str ~= nil and not force),
				}
			end,
		},
		help = {
			signature = "help(args: {model: string?, force: boolean?}?): table",
			description = "Fetch OpenRouter documentation on demand, cached for 24h (unless force=true). Specify model (e.g. 'qwen/qwen-image-2.1-turbo') to fetch its llms.txt guide. Omit model to fetch the root documentation index (docs/llms.txt).",
			run = function(args)
				args = args or {}
				local force = args.force == true
				local model = args.model

				if model and #model > 0 and model ~= "docs" then
					local provider, model_id = model:match("^([^/]+)/(.+)$")
					assert(provider and model_id, "model must be in 'provider/model-id' format (e.g. 'qwen/qwen-image-2.1-turbo')")

					local sanitized = (provider .. "_" .. model_id):gsub("[^%w_%-]", "_")
					local cache_path = ".openrouter/help/" .. sanitized .. ".txt"
					local cached = read_cache(cache_path, force)
					if cached then
						return {
							content = cached,
							model = model,
							from_cache = true,
						}
					end

					local res = http("model_help", { provider = provider, model_id = model_id })
					local content = res and res.text or ""
					if #content > 0 then
						write_cache(cache_path, content)
					end
					return {
						content = content,
						model = model,
						from_cache = false,
					}
				else
					local cache_path = ".openrouter/help/docs.txt"
					local cached = read_cache(cache_path, force)
					if cached then
						return {
							content = cached,
							from_cache = true,
						}
					end

					local res = http("docs_index", {})
					local content = res and res.text or ""
					if #content > 0 then
						write_cache(cache_path, content)
					end
					return {
						content = content,
						from_cache = false,
					}
				end
			end,
		},
		text = {
			signature = "text(args: {model: string, prompt: string?, messages: table?, temperature: number?, max_tokens: number?, top_p: number?, response_format: table?, provider: table?}): table",
			description = "Generate text via OpenRouter Chat Completions API. Pass either prompt (string) or messages (array of {role, content}). Returns {id, text, usage, raw}.",
			run = function(args)
				assert(type(args) == "table" and type(args.model) == "string" and args.model:match("%S"), "openrouter.text requires a model")
				local messages = args.messages
				if not messages then
					assert(type(args.prompt) == "string" and args.prompt:match("%S"), "openrouter.text requires prompt or messages")
					messages = {
						{ role = "user", content = args.prompt },
					}
				end
				local payload = {
					model = args.model,
					messages = messages,
					temperature = args.temperature,
					max_tokens = args.max_tokens,
					top_p = args.top_p,
					response_format = args.response_format,
					provider = args.provider,
				}
				local res = http("chat", payload)
				local choice = res.choices and res.choices[1]
				local content = choice and choice.message and choice.message.content or ""
				return {
					id = res.id,
					text = content,
					usage = res.usage,
					model = res.model,
					raw = res,
				}
			end,
		},
		image = {
			signature = "image(args: {model: string, prompt: string, aspect_ratio: string?, resolution: string?, n: number?, seed: number?, input_references: table?, save_to: string?}): table",
			description = "Generate images via OpenRouter Images API (e.g. 'qwen/qwen-image-2.1-turbo', 'black-forest-labs/flux-1-schnell'). Supports aspect_ratio ('16:9','1:1'), resolution ('1K','2K'), n (1..6). If save_to is provided, decodes base64 and saves binary image bytes to disk.",
			run = function(args)
				assert(type(args) == "table" and type(args.model) == "string" and args.model:match("%S"), "openrouter.image requires a model")
				assert(type(args.prompt) == "string" and args.prompt:match("%S"), "openrouter.image requires a prompt")
				local payload = {
					model = args.model,
					prompt = args.prompt,
					aspect_ratio = args.aspect_ratio,
					resolution = args.resolution,
					n = args.n or 1,
					seed = args.seed,
					input_references = args.input_references,
				}
				local res = http("images", payload)
				local saved_path = nil
				if args.save_to and res.data and res.data[1] and res.data[1].b64_json then
					local decoded = base64_decode(res.data[1].b64_json)
					local ok, _ = pcall(write, args.save_to, decoded)
					if ok then
						saved_path = args.save_to
					end
				end
				return {
					images = res.data or {},
					usage = res.usage,
					saved_to = saved_path,
					raw = res,
				}
			end,
		},
		sound = {
			signature = "sound(args: {model: string, input: string, voice: string?, response_format: string?, speed: number?, save_to: string?}): table",
			description = "Generate speech audio via OpenRouter Text-to-Speech API (e.g. 'elevenlabs/eleven-v4', 'heygen/heygen-voice-1'). Supports voice, speed, response_format ('mp3','pcm'). If save_to is specified, decodes base64 and saves binary audio bytes to disk.",
			run = function(args)
				assert(type(args) == "table" and type(args.model) == "string" and args.model:match("%S"), "openrouter.sound requires a model")
				assert(type(args.input) == "string" and args.input:match("%S"), "openrouter.sound requires input text")
				local payload = {
					model = args.model,
					input = args.input,
					voice = args.voice,
					response_format = args.response_format or "mp3",
					speed = args.speed,
				}
				local res = http("speech", payload)
				local saved_path = nil
				if args.save_to and res and res.base64 then
					local decoded = base64_decode(res.base64)
					local ok, _ = pcall(write, args.save_to, decoded)
					if ok then
						saved_path = args.save_to
					end
				end
				return {
					base64 = res and res.base64,
					media_type = res and res.media_type,
					saved_to = saved_path,
				}
			end,
		},
		video = {
			signature = "video(args: {model: string, prompt: string?, duration: number?, resolution: string?, aspect_ratio: string?, frame_images: table?, input_references: table?}): table",
			description = "Submit asynchronous video generation request via OpenRouter Videos API (e.g. 'minimax/hailuo-3-max', 'google/veo-3.1'). Returns job object with id and polling_url. Poll with poll_video{job_id=id}.",
			run = function(args)
				assert(type(args) == "table" and type(args.model) == "string" and args.model:match("%S"), "openrouter.video requires a model")
				assert((type(args.prompt) == "string" and args.prompt:match("%S")) or type(args.frame_images) == "table", "openrouter.video requires prompt or frame_images")
				local payload = {
					model = args.model,
					prompt = args.prompt,
					duration = args.duration,
					resolution = args.resolution,
					aspect_ratio = args.aspect_ratio,
					frame_images = args.frame_images,
					input_references = args.input_references,
				}
				local res = http("videos", payload)
				return {
					id = res.id,
					generation_id = res.generation_id,
					status = res.status,
					polling_url = res.polling_url,
					raw = res,
				}
			end,
		},
		poll_video = {
			signature = "poll_video(args: {job_id: string}): table",
			description = "Poll status and retrieve download URLs of an OpenRouter video generation job. Returns {id, status, urls, usage, error}.",
			run = function(args)
				assert(type(args) == "table" and type(args.job_id) == "string" and args.job_id:match("%S"), "openrouter.poll_video requires job_id")
				local res = http("poll_video", { job_id = args.job_id })
				return {
					id = res.id,
					status = res.status,
					urls = res.unsigned_urls or {},
					usage = res.usage,
					error = res.error,
					raw = res,
				}
			end,
		},
	},
}
