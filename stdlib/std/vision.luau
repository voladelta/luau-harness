-- Opt-in pack (--pack std/vision): lets a vision model look at images (rendered frames, contact sheets, screenshots).
--   view(path | {paths}, {max = 1024}) -> { {path, width, height} }
-- The pixels are not returned to Luau: they are queued and sent to the model as one user message before its next turn.
-- Images stay in the history (dropping them would break the prompt cache), so a turn takes at most 4.
local M = {}

local MAX_PER_CALL = 4

local DOCS = [==[
# Looking at images
- view(path | {paths}, {max = 1024}) -> { {path, width, height} }  shows you PNG/JPEG/WebP/GIF files on your next turn
  (at most 4 per turn, scaled so the long side is at most `max`, 64..2048). Use it to check rendered output instead of
  guessing; prefer one contact sheet over many single frames. Each image costs about 1-2k input tokens.
]==]

function M.install(ctx)
	local pending = {}

	hx.expose("view", function(paths, opts)
		if type(paths) == "string" then
			paths = { paths }
		end
		if type(paths) ~= "table" or #paths == 0 then
			error("view: pass a path or a list of paths", 2)
		end
		if #pending + #paths > MAX_PER_CALL then
			error("view: at most " .. MAX_PER_CALL .. " images per turn (" .. #pending .. " already queued, " .. #paths .. " more asked); make a contact sheet instead", 2)
		end
		local max = if type(opts) == "table" then opts.max else nil
		local out = {}
		for _, p in paths do
			local img = hx.view_image(tostring(p), max)
			table.insert(pending, img)
			table.insert(out, { path = img.path, width = img.width, height = img.height })
		end
		return out
	end)

	ctx.hooks.on("before_model", function(req)
		if #pending > 0 then
			local names, images = {}, {}
			for _, img in pending do
				table.insert(names, img.path .. " (" .. img.width .. "x" .. img.height .. ")")
				table.insert(images, { mime = img.mime, data = img.data })
			end
			-- appended to the conversation itself (req.messages is the loop's table), so later turns keep it
			table.insert(req.messages, { role = "user", content = "Images from view(), in order: " .. table.concat(names, ", "), images = images })
			hx.log("### VIEW\n" .. table.concat(names, "\n"))
			pending = {}
		end
		req.system = req.system .. "\n\n" .. DOCS
		return req
	end)
end

return M
