-- Opt-in pack (--pack std/vision): lets a vision model look at images (rendered frames, contact sheets, screenshots).
--   view(path | {paths}, {max = 1024}) -> { {path, width, height} }
-- The pixels are not returned to Luau: they are queued and sent to the model as one user message before its next turn.
-- Images stay in the history (dropping them would break the prompt cache) until a compaction summarizes them; std/compact
-- also compacts when they pass its image budget. One model reply takes at most 4, however many calls it makes, so the
-- newest exchange always fits that budget.
local M = {}

M.MAX_PER_CALL = 4
-- base64 bytes one model reply may add. std/compact's image budget is twice this (a compaction keeps at most half of it),
-- so a compaction frees room for the next turn instead of recurring every turn. The controller state carries the
-- images across a data boundary of hx.limits.DATA_BYTES; before a compaction it holds at most the budget plus one
-- reply's images, three times this, which leaves half the boundary for the text.
M.MAX_BYTES_PER_TURN = hx.limits.DATA_BYTES // 6
local MAX_PER_CALL, MAX_BYTES_PER_TURN = M.MAX_PER_CALL, M.MAX_BYTES_PER_TURN
local MB = 2 ^ 20

-- The prompt section, from the limits in force: these constants and the host's scaling bounds (`hx.limits`).
local function docs(): string
	local l = hx.limits
	return string.format([==[
# Looking at images
- view(path | {paths}, {max = %d}) -> { {path, width, height} }  shows you PNG/JPEG/WebP/GIF files on your next turn
  (at most %d and %.1f MB per turn, scaled so the long side is at most `max`, %d..%d). Use it to check rendered output instead of
  guessing; prefer one contact sheet over many single frames. Each image costs about 1-2k input tokens.
]==], l.VIEW_MAX, MAX_PER_CALL, MAX_BYTES_PER_TURN / MB, l.VIEW_MAX_MIN, l.VIEW_MAX_LIMIT)
end

function M.install(ctx)
	local pending = {} -- loaded images not yet handed to the loop
	-- What view() took since the last agent request. The host collects `pending` after every chunk, so the limits
	-- count here: one reply's parallel calls share them.
	local count, bytes = 0, 0

	hx.expose("view", function(paths, opts)
		if type(paths) == "string" then
			paths = { paths }
		end
		if type(paths) ~= "table" or #paths == 0 then
			error("view: pass a path or a list of paths", 2)
		end
		if count + #paths > MAX_PER_CALL then
			error("view: at most " .. MAX_PER_CALL .. " images per turn (" .. count .. " already viewed, " .. #paths .. " more asked); make a contact sheet instead", 2)
		end
		local max = if type(opts) == "table" then opts.max else nil
		local total = bytes
		local loaded = {}
		for _, p in paths do
			local img = hx.view_image(tostring(p), max)
			total += #img.data
			if total > MAX_BYTES_PER_TURN then
				error(string.format("view: these images are %.1f MB, over the %.1f MB a turn takes; use a smaller max or a contact sheet",
					total / MB, MAX_BYTES_PER_TURN / MB), 2)
			end
			table.insert(loaded, img)
		end
		count, bytes = count + #loaded, total
		local out = {}
		for _, img in loaded do
			table.insert(pending, img)
			table.insert(out, { path = img.path, width = img.width, height = img.height })
		end
		return out
	end)

	ctx.hooks.on("before_model", function(req)
		-- An agent request always has messages; the host's collection after a chunk sends none. Only an agent request
		-- starts a new turn.
		if #req.messages > 0 then count, bytes = 0, 0 end
		if #pending > 0 then
			local names, images = {}, {}
			for _, img in pending do
				table.insert(names, img.path .. " (" .. img.width .. "x" .. img.height .. ")")
				table.insert(images, { mime = img.mime, data = img.data, path = img.path, width = img.width, height = img.height })
			end
			-- appended to the conversation itself, so later turns keep it
			table.insert(req.messages, { role = "user", content = "Images from view(), in order: " .. table.concat(names, ", "), images = images })
			hx.log("### VIEW\n" .. table.concat(names, "\n"))
			pending = {}
		end
		req.system = req.system .. "\n\n" .. docs()
		return req
	end)
end

return M
