-- Arm "musk": a high-leverage working method appended to the system prompt on every turn (Musk's five-step algorithm).
local method = [[

# Method
When you choose an approach, and again whenever progress stalls:
1. Question every requirement. Separate what the task actually states from what you assumed; drop the assumptions.
2. Delete steps, code and parts the stated requirements do not need.
3. Simplify and optimize only what remains.
4. Shorten the loop: make each check fast so you can try, measure and correct quickly.
5. Automate last.
First ask: which single decision or fact, settled now, makes the rest simpler or unblocks it? Settle that first.]]

require("std/hooks").on("before_model", function(req)
	req.system ..= method
	return req
end)
