-- The judge, built into hx. With --judge KIT, hx forks a process for it before the agent's wall goes up; the process
-- puts up a wall of its own, which reads the project and the kit and writes only the judge's state directory. The
-- agent cannot read the kit or the state, and reaches the judge only through host.judge: each line in is a
-- submission, {"note": text}, and each line out is the verdict, as a JSON string.
--
-- The rule it keeps: the agent proposes, the kit measures. A submission is a copy of the project as it stands. The
-- kit's build makes it, the kit's run is timed by the judge from outside, and the kit's check decides whether the
-- output is right and may report a score. The candidate and the champion, the best version so far, are measured in
-- pairs that alternate in a random order, so both meet the same machine. A change wins when it is better by more
-- than the margin in the median pair and in all pairs but one, and then again on fresh pairs. Only then is it the
-- champion. Every trial goes into the ledger, with its patch, whatever its verdict.
--
-- The kit is a directory:
--   run       executable, required. Runs in a copy of the candidate, behind a wall that holds only that copy, the
--             system directories and the --ro and --rw paths: it cannot read the kit. Its time is the measure,
--             unless the check reports a score. It exits 0 when it worked.
--   build     executable, optional. Runs once per candidate, untimed, behind the same wall, before any run.
--   check     executable, optional. Runs after each run as `check DIR`, where DIR is the run's copy and the run's
--             output is DIR/.judge/stdout. It reads the kit and DIR and writes nothing that lasts. It exits 0 when
--             the output is right; when its last line is a number, that is the score, and higher is better.
--             What it prints when it fails goes to the agent.
--   kit.json  optional: {"runs": 5, "margin": 0.02, "timeout": 600, "blind": false}: pairs per round, the
--             smallest gain that counts, seconds allowed for one build, run or check, and whether the agent is
--             told verdicts only, without numbers, so it cannot fit what the kit holds back by reading scores.

local kit, state = host.args[1], host.args[2]
local project = host.cwd
local reads, writes = {}, {} -- --ro and --rw: the candidate's commands get what the agent's get
for i = 3, #host.args, 2 do
	table.insert(host.args[i] == "--ro" and reads or writes, host.args[i + 1])
end

local config = { runs = 5, margin = 0.02, timeout = 600, blind = false }
do
	local text = host.read(kit .. "/kit.json")
	if text then
		for key, value in json.decode(text) do
			config[key] = value
		end
	end
	config.runs = math.max(1, math.floor(config.runs))
end
math.randomseed(os.time())

local function say(text)
	if not host.quiet then
		host.log("judge: " .. text)
	end
end

local function quote(text)
	return "'" .. text:gsub("'", "'\\''") .. "'"
end

local function tail(text, limit)
	text = text:gsub("%s+$", "")
	return #text <= limit and text or "..." .. text:sub(-limit)
end

-- A command of the judge's own, in its own wall. Raises when it fails: that is the judge's fault, not the trial's.
local function shell(command)
	local result = host.sh(command, { timeout = config.timeout })
	if result.code ~= 0 then
		error(("%s: %s"):format(command, tail(result.out, 500)), 0)
	end
	return result.out
end

local has_build, has_check = host.read(kit .. "/build") ~= nil, host.read(kit .. "/check") ~= nil

-- A command of the kit's, on a candidate, behind the candidate's wall: the directory it works in, plus what the
-- agent may read and write, and never the kit.
local function candidate(dir, script)
	local wall = table.clone(writes)
	table.insert(wall, dir)
	shell(("rm -rf %s/.judge && mkdir -p %s/.judge/tmp && cp %s %s/.judge/"):format(quote(dir), quote(dir),
		quote(kit .. "/" .. script), quote(dir)))
	local started = os.clock()
	local result = host.sh(("cd %s && TMPDIR=$PWD/.judge/tmp ./.judge/%s > .judge/stdout 2>&1"):format(quote(dir),
		script), { timeout = config.timeout, read = reads, write = wall })
	local seconds = os.clock() - started
	if result.code == 0 then
		return true, seconds
	end
	local how = result.timed_out and ("timed out after %d s"):format(config.timeout)
		or result.signal and "was killed by signal " .. result.signal
		or "exited " .. result.code
	return false, ("%s %s:\n%s"):format(script, how, tail(host.read(dir .. "/.judge/stdout") or "", 1500))
end

-- Copies the project as it stands, without .git and .hx, to dir.
local function snapshot(dir)
	shell(("rm -rf %s && mkdir -p %s && tar -C %s --exclude=./.git --exclude=./.hx -cf - . | tar -C %s -xf -"):format(
		quote(dir), quote(dir), quote(project), quote(dir)))
end

-- Builds the source in dir/src into dir/built. Returns true, or false and why.
local function build(dir)
	shell(("rm -rf %s/built && cp -a %s/src %s/built"):format(quote(dir), quote(dir), quote(dir)))
	if not has_build then
		return true
	end
	local ok, why = candidate(dir .. "/built", "build")
	return ok, not ok and why or nil
end

-- One measurement of a built tree, in a fresh copy of it, so no run sees what an earlier one left. Returns
-- {value, kind}, where a higher value is better, or nil and why.
local function measure(built)
	local dir = state .. "/run"
	shell(("rm -rf %s && cp -a %s %s"):format(quote(dir), quote(built), quote(dir)))
	local ok, seconds = candidate(dir, "run")
	if not ok then
		return nil, seconds
	end
	if not has_check then
		return { value = -seconds, seconds = seconds, kind = "seconds" }
	end
	local wall = table.clone(reads)
	table.insert(wall, kit)
	table.insert(wall, dir)
	table.move(writes, 1, #writes, #wall + 1, wall)
	local result = host.sh(("cd %s && TMPDIR=%s %s %s"):format(quote(dir), quote(state .. "/tmp"),
		quote(kit .. "/check"), quote(dir)), { timeout = config.timeout, read = wall, write = { state .. "/tmp" } })
	if result.code ~= 0 then
		return nil, "the check failed:\n" .. tail(result.out, 1500)
	end
	local score = tonumber(result.out:match("([^\n]*)%s*$"))
	if score then
		return { value = score, seconds = seconds, kind = "score" }
	end
	return { value = -seconds, seconds = seconds, kind = "seconds" }
end

-- How much better a is than b, as a fraction: a speed-up for times, a relative difference for scores.
local function gain(a, b)
	if a.kind == "seconds" then
		return a.seconds > 0 and b.seconds / a.seconds - 1 or 0
	end
	return b.value ~= 0 and (a.value - b.value) / math.abs(b.value) or a.value - b.value
end

local function median(list)
	local sorted = table.clone(list)
	table.sort(sorted)
	local n = #sorted
	return n % 2 == 1 and sorted[(n + 1) // 2] or (sorted[n // 2] + sorted[n // 2 + 1]) / 2
end

local function percent(fraction)
	return ("%+.1f%%"):format(fraction * 100)
end

-- Measures the candidate beside the champion in config.runs pairs. Returns "better", "worse" or "same" with the
-- median gain, or nil and why the candidate failed.
local function round(built, champion)
	local gains = {}
	for _ = 1, config.runs do
		local mine, theirs, why
		if math.random() < 0.5 then
			mine, why = measure(built)
			theirs = mine and measure(champion.built)
		else
			theirs = measure(champion.built)
			mine, why = measure(built)
		end
		if not mine then
			return nil, why
		end
		if not theirs then
			error("the champion failed to run, so the kit or the machine is not steady", 0)
		end
		if mine.kind ~= theirs.kind then
			return nil, "the check reported a score for one and not the other"
		end
		table.insert(gains, gain(mine, theirs))
	end
	local middle, ahead = median(gains), 0
	for _, g in gains do
		ahead += g > 0 and 1 or 0
	end
	if middle > config.margin and ahead >= #gains - 1 then
		return "better", middle
	end
	return middle < -config.margin and "worse" or "same", middle
end

-- The state of the judge -----------------------------------------------------------------------------------------

local champion -- {trial, dir, built, total}; total is the gain over the start, multiplied up from the wins
local trials, ledger, counts = 0, {}, {}

local function record(entry)
	table.insert(ledger, json.encode(entry))
	host.write(state .. "/ledger.jsonl", table.concat(ledger, "\n") .. "\n")
	counts[entry.verdict] = (counts[entry.verdict] or 0) + 1
end

-- Writes dir's patch from the start: what the agent changed, with a/ and b/ paths, for `patch -p1` in the project.
local function patch(dir, to)
	local from = dir:sub(#state + 2) .. "/src/"
	host.sh(("cd %s && diff -ruN --no-dereference start/src %s | sed -e 's#^--- start/src/#--- a/#' -e 's#^+++ %s#+++ b/#' > %s")
		:format(quote(state), quote(from), from:gsub("[#\\.]", "\\%0"), quote(to)))
end

local function crown(trial, dir, total)
	if champion and champion.dir ~= state .. "/start" then
		shell("rm -rf " .. quote(champion.dir))
	end
	champion = { trial = trial, dir = dir, built = dir .. "/built", total = total }
	patch(dir, "champion.patch")
end

local function champion_name()
	return champion.trial == 0 and "the starting point" or "trial " .. champion.trial
end

local function start()
	local dir = state .. "/start"
	snapshot(dir .. "/src")
	local ok, why = build(dir)
	local result
	if ok then
		result, why = measure(dir .. "/built")
	end
	if result then
		champion = { trial = 0, dir = dir, built = dir .. "/built", total = 0 }
		say(("the starting point passes (%.3f s%s); it is the champion"):format(result.seconds,
			result.kind == "score" and ", score " .. result.value or ""))
	else
		say("the starting point does not pass, so the first submission that does becomes the champion:\n" .. why)
	end
	record({ trial = 0, verdict = result and "start" or "start fails", why = why })
end

local function submit(note)
	trials += 1
	local trial = trials
	local dir = ("%s/trials/%d"):format(state, trial)
	snapshot(dir .. "/src")
	local entry = { trial = trial, time = os.time(), note = note:sub(1, 2000) }
	local answer
	local ok, why = build(dir)
	if not ok then
		entry.verdict, entry.why = "broken", why
		answer = "broken: the build failed, so nothing was measured.\n" .. why
	elseif not champion then
		local result
		result, why = measure(dir .. "/built")
		if result then
			entry.verdict = "champion"
			crown(trial, dir, 0)
			answer = "champion: this is the first version that passes, so it is the champion now, and later "
				.. "submissions are measured against it."
		else
			entry.verdict, entry.why = "invalid", why
			answer = "invalid: nothing passes yet.\n" .. why
		end
	else
		local verdict, screen = round(dir .. "/built", champion)
		if not verdict then
			entry.verdict, entry.why = "invalid", screen
			answer = "invalid: " .. screen
		elseif verdict ~= "better" then
			entry.verdict, entry.gain = verdict == "same" and "no different" or "worse", screen
			answer = config.blind and entry.verdict .. ": not better than the champion by the margin."
				or ("%s: %s against the champion in the median of %d pairs; a change counts from %s."):format(
					entry.verdict, percent(screen), config.runs, percent(config.margin))
		else
			local confirmed, again = round(dir .. "/built", champion)
			entry.gain = screen
			if not confirmed then
				entry.verdict, entry.why = "invalid", again
				answer = "invalid on fresh runs: " .. again
			elseif confirmed == "better" then
				entry.confirm = again
				entry.verdict = "win"
				local total = (1 + champion.total) * (1 + again) - 1
				crown(trial, dir, total)
				answer = config.blind and "win: better than the champion, and again on fresh runs. This is the "
						.. "champion now."
					or ("win: %s against the champion, and %s again on %d fresh pairs. This is the champion now, "
						.. "about %s against the start."):format(percent(screen), percent(again), config.runs,
						percent(total))
			else
				entry.verdict, entry.confirm = "not confirmed", again
				answer = config.blind and "not confirmed: better at first, but not on fresh runs."
					or ("not confirmed: %s in the first %d pairs, but %s on fresh ones."):format(percent(screen),
						config.runs, percent(again))
			end
		end
	end
	record(entry)
	patch(dir, ("trials/%d.patch"):format(trial))
	if entry.verdict ~= "win" and entry.verdict ~= "champion" then
		shell("rm -rf " .. quote(dir))
	else
		shell("rm -rf " .. quote(dir .. "/built/.judge"))
	end
	if champion and entry.verdict ~= "win" and entry.verdict ~= "champion" then
		answer ..= ("\nThe champion is still %s."):format(champion_name())
	end
	say(("trial %d: %s"):format(trial, entry.verdict))
	return ("Trial %d. %s"):format(trial, answer)
end

local function summary()
	local parts = {}
	for _, verdict in { "win", "champion", "not confirmed", "no different", "worse", "invalid", "broken" } do
		if counts[verdict] then
			table.insert(parts, ("%d %s"):format(counts[verdict], verdict))
		end
	end
	local lines = { ("hx judge: %d trial%s%s"):format(trials, trials == 1 and "" or "s",
		#parts > 0 and ": " .. table.concat(parts, ", ") or "") }
	if champion and champion.trial > 0 then
		table.insert(lines, ("the champion is trial %d, about %s against the start; its patch is %s/champion.patch")
			:format(champion.trial, percent(champion.total), state))
	elseif champion then
		table.insert(lines, "nothing beat the starting point")
	end
	table.insert(lines, ("the ledger is %s/ledger.jsonl"):format(state))
	host.log(table.concat(lines, "\n"))
	host.sh(("rm -rf %s/run %s/tmp"):format(quote(state), quote(state)))
end

local ok, err = host.try(start)
if not ok then
	host.log("hx: judge: cannot measure the starting point: " .. tostring(err))
end
while true do
	local line = host.input()
	if not line then
		break
	end
	local decoded, request = pcall(json.decode, line)
	local note = decoded and type(request) == "table" and type(request.note) == "string" and request.note or ""
	local done, answer = host.try(submit, note)
	if not done then
		answer = "the judge could not finish this trial, through no fault of the submission: " .. tostring(answer)
		say(answer)
	end
	print(json.encode(answer))
end
summary()
