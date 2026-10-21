-- String Seed of Thought (https://pub.sakana.ai/ssot/) for diverse candidates from the run's own model.
-- Copy to .harness/tools/ssot.lua, then run hx trust.
--
-- Each candidate gets a fresh host seed. The model lists the root-level dimensions along which answers differ, picks one
-- option per dimension from a different part of the seed, then answers. In blind tests on DeepSeek V4.1 Flash this, not
-- the paper's self-generated string, is what moved answers off the model's default: the model's own "random" strings
-- repeat across calls, and a seed alone only reshuffles surface details. `hx run --ar` explore workers derive their
-- ideas the same way, from a host seed (src/ssot.rs).

local function instructions(seed)
	return "You must produce exactly one unique and diverse answer. Your random string is "
		.. "<random_string>" .. seed .. "</random_string>. Within <thinking> and </thinking> tags: first list "
		.. "5 to 7 root-level dimensions along which answers to this task can differ fundamentally "
		.. "(not surface details), each with at least 6 options ranging from conventional to radical. "
		.. "Then pick one option per dimension using a different part of the random string "
		.. "(e.g. sum of character codes of a segment, mod the number of options). Commit to the "
		.. "picks even if unusual, and make them cohere.\n"
		.. "Then, provide your final answer, enclosed within <answer> and </answer> tags."
end

return {
	capabilities = {
		llm = { "chat" },
	},
	tools = {
		sample = {
			signature = "sample(args: {prompt: string, n: number?, effort: string?, max_tokens: number?}): {answers: {{answer: string, seed: string}}, errors: {string}}",
			description = "Get n (default 5, max 32) deliberately different answers to one open-ended prompt (ideas, designs, angles, drafts) from the run's model, in parallel. Write the prompt for ONE answer (\"Propose a layout for...\", not \"Give 6 layouts\"); n sets how many. Each answer is steered by its own random seed toward a different combination of root-level choices, so expect some unusual or weak ones: judge and filter them yourself. effort defaults to \"none\" (no reasoning, which gave the most variety); pass low..max to override. Not for tasks with one correct answer.",
			run = function(args)
				assert(type(args) == "table" and type(args.prompt) == "string" and args.prompt:match("%S"), "ssot.sample requires a non-empty prompt")
				local n = args.n or 5
				assert(type(n) == "number" and n == math.floor(n) and n >= 1 and n <= 32, "ssot.sample: n must be an integer from 1 to 32")
				local requests, seeds = {}, {}
				for i = 1, n do
					seeds[i] = nonce()
					requests[i] = { system = instructions(seeds[i]), prompt = args.prompt, effort = args.effort or "none", max_tokens = args.max_tokens }
				end
				local answers, errors = {}, {}
				for i, result in llm(requests) do
					-- The last tagged answer wins: a model may quote the tags while reasoning. Untagged text after </thinking> is
					-- an answer only when no <answer> was opened (an opened one without a close was cut off).
					local answer
					if result.ok then
						for tagged in result.value:gmatch("<answer>(.-)</answer>") do answer = tagged end
						answer = answer or (not result.value:find("<answer>", 1, true) and result.value:match("</thinking>%s*(.-)%s*$"))
					end
					if answer and answer:match("%S") then
						table.insert(answers, { answer = answer:match("^%s*(.-)%s*$"), seed = seeds[i] })
					else
						table.insert(errors, result.ok and "no answer in reply (cut off by max_tokens?)" or result.error)
					end
				end
				return { answers = answers, errors = errors }
			end,
		},
	},
}
