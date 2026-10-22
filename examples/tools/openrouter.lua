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
-- - Responses: https://openrouter.ai/api/v1/responses
-- - Images: https://openrouter.ai/api/v1/images
-- - Audio: https://openrouter.ai/api/v1/audio/speech
-- - Videos: https://openrouter.ai/api/v1/videos

local ORIGIN = "https://openrouter.ai"

-- Every endpoint shares the origin and the bearer key; only path, method and flags differ.
local function endpoint(path, method, flags)
	local spec = { url = ORIGIN .. path, method = method, secret = "OPENROUTER_API_KEY", bearer = true }
	for k, v in pairs(flags or {}) do
		spec[k] = v
	end
	return spec
end

-- save_to for the first file, save_to with -2, -3 ... before the extension for the rest.
local function numbered(path, i)
	if i == 1 then return path end
	local stem, ext = path:match("^(.-)(%.[^./]*)$")
	return stem and (stem .. "-" .. i .. ext) or (path .. "-" .. i)
end

local function read_cache(path, force)
	if force then return nil end
	local ok, raw = pcall(read, path)
	if not ok or type(raw) ~= "string" or #raw == 0 then
		return nil
	end
	local nl = raw:find("\n")
	if not nl then return nil end
	local age = os.time() - (tonumber(raw:sub(1, nl - 1)) or -1)
	-- A timestamp from the future (clock skew, hand edit) is stale, not valid forever.
	if age >= 0 and age < 86400 then
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
		credits = endpoint("/api/v1/credits", "GET", { read_only = true }),
		models = endpoint("/api/v1/models", "GET", { read_only = true }),
		model_help = endpoint("/{provider}/{model_id}/llms.txt", "GET", { raw = true, read_only = true }),
		docs_index = endpoint("/docs/llms.txt", "GET", { raw = true, read_only = true }),
		responses = endpoint("/api/v1/responses", "POST"),
		images = endpoint("/api/v1/images", "POST"),
		speech = endpoint("/api/v1/audio/speech", "POST", { raw = true }),
		videos = endpoint("/api/v1/videos", "POST"),
		poll_video = endpoint("/api/v1/videos/{job_id}", "GET", { read_only = true }),
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
					local ok, parsed = pcall(json.decode, cached_str)
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
			signature = "text(args: {model: string, prompt: string?, messages: table?, temperature: number?, max_output_tokens: number?, top_p: number?, text: table?, provider: table?}): table",
			description = "Generate text via OpenRouter Responses API. Pass either prompt (string) or messages (array of {role, content}). Returns {id, text, usage, raw}.",
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
					input = messages,
					temperature = args.temperature,
					max_output_tokens = args.max_output_tokens,
					top_p = args.top_p,
					text = args.text,
					provider = args.provider,
				}
				local res = http("responses", payload)
				local parts = {}
				for _, item in ipairs(res.output or {}) do
					if item.type == "message" then
						for _, part in ipairs(item.content or {}) do
							if part.type == "output_text" then table.insert(parts, part.text) end
						end
					end
				end
				local content = table.concat(parts, "\n")
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
			description = "Generate images via OpenRouter Images API (e.g. 'qwen/qwen-image-2.1-turbo', 'black-forest-labs/flux-1-schnell'). Supports aspect_ratio ('16:9','1:1'), resolution ('1K','2K'), n (1..6). If save_to is given, saves every image (save_to, then name-2.ext, ...) and returns saved_to as a list of paths with b64_json removed; a failed write raises.",
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
				local images = res.data or {}
				local saved_to = nil
				if args.save_to then
					saved_to = {}
					for i, img in ipairs(images) do
						assert(img.b64_json, "openrouter.image: response has no b64_json to save")
						local path = numbered(args.save_to, i)
						write(path, base64.decode(img.b64_json)) -- raises on failure; never report an unsaved file
						saved_to[i] = path
						img.b64_json = nil -- the bytes are on disk; keep megabytes out of the caller's data
					end
				end
				return {
					images = images,
					usage = res.usage,
					saved_to = saved_to,
				}
			end,
		},
		sound = {
			signature = "sound(args: {model: string, input: string, voice: string?, response_format: string?, speed: number?, save_to: string?}): table",
			description = "Generate speech audio via OpenRouter Text-to-Speech API (e.g. 'elevenlabs/eleven-v4', 'heygen/heygen-voice-1'). Supports voice, speed, response_format ('mp3','pcm'). If save_to is given, saves the audio there and omits base64 from the result; a failed write raises.",
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
				if args.save_to then
					assert(res.base64, "openrouter.sound: response was not audio (" .. tostring(res.media_type) .. ")")
					write(args.save_to, base64.decode(res.base64)) -- raises on failure
				end
				return {
					base64 = not args.save_to and res.base64 or nil,
					media_type = res.media_type,
					saved_to = args.save_to,
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
