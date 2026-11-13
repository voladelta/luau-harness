-- Opt-in pack (--pack std/vision): lets a vision model look at images (rendered frames, contact sheets, screenshots).
--   view(path | {paths}, {max = 1024}) -> { {path, width, height} }
-- The pixels are not returned to Luau: they are queued, handed to the controller after the chunk and sent to the model as
-- one user message per chunk before its next turn.
-- Images stay in the history (dropping them would break the prompt cache) until a compaction summarizes them; std/compact
-- also compacts when they pass its image budget. A turn takes at most 4, so the newest exchange always fits that budget.
local M = {}

M.MAX_PER_CALL = 4
-- base64 bytes one turn may add, across its chunks. The history, images included, is controller state, which the host
-- caps at 16 MiB (`hx.limits.STATE_BYTES` in the controller): std/compact's image budget is twice this (a compaction
-- keeps at most half of it, so it frees room for the next turn instead of recurring every turn), and a turn's images
-- on top of that budget stay within three quarters of the cap.
M.MAX_BYTES_PER_TURN = 4 * 1024 * 1024
local MAX_PER_CALL, MAX_BYTES_PER_TURN = M.MAX_PER_CALL, M.MAX_BYTES_PER_TURN

-- The prompt section, from the limits in force: these constants and the host's scaling bounds (`hx.limits`).
local function docs(): string
	local l = hx.limits
	return string.format([==[
# Looking at images
- view(path | {paths}, {max = %d}) -> { {path, width, height} }  shows you PNG/JPEG/WebP/GIF files on your next turn
  (at most %d and %d MB per turn, scaled so the long side is at most `max`, %d..%d). Use it to check rendered output instead of
  guessing; prefer one contact sheet over many single frames. Each image costs about 1-2k input tokens.
]==], l.VIEW_MAX, MAX_PER_CALL, MAX_BYTES_PER_TURN // 2 ^ 20, l.VIEW_MAX_MIN, l.VIEW_MAX_LIMIT)
end

function M.install(ctx)
	local pending = {}
	-- images this turn has taken so far: the host drains `pending` after every chunk, a turn ends at the next request
	local count, bytes = 0, 0

	hx.expose("view", function(paths, opts)
		if type(paths) == "string" then
			paths = { paths }
		end
		if type(paths) ~= "table" or #paths == 0 then
			error("view: pass a path or a list of paths", 2)
		end
		if count + #paths > MAX_PER_CALL then
			error("view: at most " .. MAX_PER_CALL .. " images per turn (" .. count .. " already queued, " .. #paths .. " more asked); make a contact sheet instead", 2)
		end
		local max = if type(opts) == "table" then opts.max else nil
		local total = bytes
		local loaded = {}
		for _, p in paths do
			local img = hx.view_image(tostring(p), max)
			total += #img.data
			if total > MAX_BYTES_PER_TURN then
				error(string.format("view: these images are %.1f MB, over the %d MB a turn takes; use a smaller max or a contact sheet",
					total / 2 ^ 20, MAX_BYTES_PER_TURN / 2 ^ 20), 2)
			end
			table.insert(loaded, img)
		end
		local out = {}
		for _, img in loaded do
			table.insert(pending, img)
			table.insert(out, { path = img.path, width = img.width, height = img.height })
		end
		count, bytes = count + #loaded, total
		return out
	end)

	-- the queued images as one user message, once
	local function take()
		if #pending == 0 then return {} end
		local names, images = {}, {}
		for _, img in pending do
			table.insert(names, img.path .. " (" .. img.width .. "x" .. img.height .. ")")
			table.insert(images, { mime = img.mime, data = img.data, path = img.path, width = img.width, height = img.height })
		end
		hx.log("### VIEW\n" .. table.concat(names, "\n"))
		pending = {}
		return { { role = "user", content = "Images from view(), in order: " .. table.concat(names, ", "), images = images } }
	end
	ctx.hooks.source(take)

	ctx.hooks.on("before_model", function(req)
		-- normally drained already; a host that does not drain (hx exec) gets them here, in the conversation itself
		for _, message in take() do table.insert(req.messages, message) end
		count, bytes = 0, 0
		req.system = req.system .. "\n\n" .. docs()
		return req
	end)
end

return M
