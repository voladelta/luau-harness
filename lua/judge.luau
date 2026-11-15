--!strict
-- The judge, built into hx. With --judge KIT, hx forks a process for it before the agent's wall goes up; the process
-- puts up a wall of its own, which reads the project and the kit and writes only the judge's state directory. The
-- agent cannot read the kit or the state, and reaches the judge only through host.judge: each line in is a
-- submission, {"note": text}, and each line out is its verdict, as a JSON string. hx waits for the first line,
-- "ready" once the starting point is measured, or why the judge cannot start, before the agent begins.
--
-- The rule it keeps: the agent proposes, the kit measures. A submission is a copy of the project as it stands. The
-- kit's build makes it, the kit's run is timed by hx from outside, and the kit's check decides whether the output
-- is right and may report a score. The candidate and the champion, the best version so far, are measured in pairs
-- that alternate in a random order, so both meet the same machine. A change wins when it is better by more than
-- the margin in the median pair and in all pairs but one, and then again on fresh pairs. Only then is it the
-- champion. Every trial goes into the ledger with its patch, whatever its verdict, errors included.
--
-- The kit is a directory:
--   run       executable, required. Runs in a fresh copy of the built candidate, behind a wall that holds only that
--             copy, the system directories and the --ro and --rw paths. Its time is the measure, unless
--             the check reports a score. It exits 0 when it worked. The candidate's code can read it while it runs.
--   build     executable, optional. Runs once per candidate, untimed, behind the same wall.
--   check     executable, optional. Runs after each run as `check DIR`, where DIR is the run's copy and the run's
--             output is DIR/.judge/stdout. It reads the kit and DIR and writes nothing that lasts. It exits 0 when
--             the output is right; when its last line is a number, that is the score: positive, and higher is
--             better. What it prints when it fails goes to the agent.
--   kit.json  optional: {"runs": 5, "margin": 0.02, "timeout": 600, "blind": false}: pairs per round, the
--             smallest relative gain that counts, seconds allowed for one build, run or check, and whether the
--             agent is told verdicts without numbers or the candidate's own output, so that it cannot fit what the
--             kit holds back by reading them.

-- What kit.json sets.
type Config = { runs: number, margin: number, timeout: number, blind: boolean }

-- One measurement of a candidate: its time ("seconds", lower is better) or the check's "score" (higher is better).
type Measure = { kind: string, value: number }

-- A round of pairs: the median gain of the candidate over the champion, and each side's values.
type Round = { gain: number, kind: string, mine: { number }, theirs: { number } }

-- The best version so far: its trial (0 for the start), the tree it was built in, the build, and its latest
-- measured median.
type Champion = { trial: number, dir: string, built: string, kind: string, value: number }

-- A line of the ledger. The verdict and what goes with it are added as the trial goes on.
type Entry = {
	trial: number,
	time: number?,
	note: string?,
	verdict: string?,
	why: string?,
	value: number?,
	screen: Round?,
	confirm: Round?,
}

local kit: string, state: string = host.args[1], host.args[2]
local project: string = host.cwd
local reads: { string }, writes: { string } = {}, {} -- --ro and --rw: the candidate's commands get what the agent's get
for i = 3, #host.args, 2 do
	table.insert(host.args[i] == "--ro" and reads or writes, host.args[i + 1])
end

-- kit.json's keys, each with the value it takes when left out. A key's type is the type of its value here.
local defaults: { [string]: number | boolean } = { runs = 5, margin = 0.02, timeout = 600, blind = false }
local config: Config -- set by main, before anything is measured

-- The kit's config, or nil and why kit.json is wrong. The result is a Config: it has every key of defaults, each
-- with a value of the type of the default.
local function read_config(): (Config?, ...string)
	local result: { [string]: any } = table.clone(defaults)
	local text = host.read(kit .. "/kit.json")
	if not text then
		return result :: Config
	end
	local given, err = json.decode(text) -- not JSON: nil and the reason, never an error raised
	if type(given) ~= "table" then
		return nil, "kit.json is not a JSON object" .. (err and ": " .. err or "")
	end
	for key, value in given do
		if defaults[key] == nil then
			return nil, ("kit.json: unknown key %s; the keys are runs, margin, timeout and blind"):format(tostring(key))
		end
		if type(value) ~= type(defaults[key]) then
			return nil, ("kit.json: %s must be a %s"):format(key, type(defaults[key]))
		end
		result[key] = value
	end
	if result.runs < 1 or result.runs % 1 ~= 0 then
		return nil, "kit.json: runs must be a whole number, 1 or more"
	end
	if not (result.margin >= 0) or not (result.timeout > 0) then
		return nil, "kit.json: margin must be 0 or more, and timeout more than 0"
	end
	return result :: Config
end

local function say(text: string)
	if not host.quiet then
		host.log("judge: " .. text)
	end
end

local function quote(text: string): string
	return "'" .. text:gsub("'", "'\\''") .. "'"
end

local function tail(text: string, limit: number): string
	text = text:gsub("%s+$", "")
	return #text <= limit and text or "..." .. text:sub(-limit)
end

-- A command of the judge's own, in its own wall. Raises when it fails: that is the judge's fault, not the trial's.
-- Its time limit is the judge's, not the kit's, and it ends when hx hangs up.
local function shell(command: string): string
	local result = host.sh("set -o pipefail; " .. command, { timeout = 3600, hangup = true })
	if result.code ~= 0 then
		error(("%s: %s"):format(command, tail(result.out, 500)), 0)
	end
	return result.out
end

-- Trees and patches -----------------------------------------------------------------------------------------------
--
-- Each snapshot is stored in a git repository of the judge's own, so a patch carries what a plain diff loses:
-- binary files, symbolic links, modes and odd names. `git apply` takes it in the project.

local git_dir = state .. "/git"

local function git(args: string, env: string?): string
	return shell(("%sGIT_CONFIG_NOSYSTEM=1 git --git-dir=%s -c core.autocrlf=false -c core.symlinks=true "
		.. "-c core.fileMode=true %s"):format(env or "", quote(git_dir), args))
end

-- The tree of a directory's files, every one of them, as git stores it.
local function tree(dir: string): string
	local index = state .. "/index"
	shell(("rm -f %s %s.lock"):format(quote(index), quote(index)))
	local env = ("cd %s && GIT_INDEX_FILE=%s "):format(quote(dir), quote(index))
	git("--work-tree=. add --all --force .", env)
	-- write-tree prints the tree's hash; had it failed, git would have raised.
	return git("write-tree", env):match("%x+") :: string
end

local start_tree: string -- set by start, before any patch is written

local function write_patch(to_tree: string, name: string)
	git(("diff --binary --full-index --no-color --no-ext-diff --no-textconv --src-prefix=a/ --dst-prefix=b/ %s %s > %s")
		:format(start_tree, to_tree, quote(state .. "/" .. name)))
end

-- Candidates ------------------------------------------------------------------------------------------------------

-- A command of the kit's, on a candidate, behind the candidate's wall: the directory it works in, plus what the
-- agent may read and write, and never the kit. Returns the seconds it took, or nil and why it failed.
local function candidate(dir: string, script: string): (number?, ...string)
	local wall = table.clone(writes)
	table.insert(wall, dir)
	shell(("rm -rf %s/.judge && mkdir -p %s/.judge/tmp && cp %s %s/.judge/"):format(quote(dir), quote(dir),
		quote(kit .. "/" .. script), quote(dir)))
	local result = host.sh(("cd %s && TMPDIR=$PWD/.judge/tmp ./.judge/%s > .judge/stdout 2>&1"):format(quote(dir),
		script), {
		timeout = config.timeout,
		read = reads,
		write = wall,
		hangup = true,
	})
	if result.code == 0 then
		return result.seconds
	end
	local how = result.timed_out and ("timed out after %g s"):format(config.timeout)
		or result.signal and "was killed by signal " .. result.signal
		or "exited " .. result.code
	if config.blind then
		return nil, ("%s %s"):format(script, how)
	end
	return nil, ("%s %s:\n%s"):format(script, how, tail(host.read(dir .. "/.judge/stdout") or "", 1500))
end

-- Copies the project as it stands, without .git and .hx, to dir.
local function snapshot(dir: string)
	shell(("rm -rf %s && mkdir -p %s && tar -C %s --exclude=./.git --exclude=./.hx -cf - . | tar -C %s -xf -"):format(
		quote(dir), quote(dir), quote(project), quote(dir)))
end

-- Every build and run happens at this one path, so a build that records where it is (cmake, a virtualenv)
-- still works when its copy is run.
local workdir = state .. "/run"

-- Builds the source in dir/src into dir/built. Returns true, or false and why.
local function build(dir: string): (boolean, ...string)
	shell(("rm -rf %s %s/built && cp -a %s/src %s"):format(quote(workdir), quote(dir), quote(dir), quote(workdir)))
	if host.read(kit .. "/build") then
		local seconds, why = candidate(workdir, "build")
		if not seconds then
			return false, why
		end
	end
	shell(("mv %s %s/built"):format(quote(workdir), quote(dir)))
	return true
end

-- One measurement of a built tree, in a fresh copy of it, so no run sees the files an earlier one left. Returns
-- {kind, value}, where kind is "seconds" (lower is better) or "score" (higher is better), or nil and why.
local function measure(built: string): (Measure?, ...string)
	local dir = workdir
	shell(("rm -rf %s && cp -a %s %s"):format(quote(dir), quote(built), quote(dir)))
	local seconds, why = candidate(dir, "run")
	if not seconds then
		return nil, why
	end
	if not host.read(kit .. "/check") then
		return { kind = "seconds", value = seconds }
	end
	-- The check can read the kit, so it must not follow a link the candidate left into it.
	shell(("find %s -type l -delete"):format(quote(dir)))
	local wall = table.clone(reads)
	table.insert(wall, kit)
	table.insert(wall, dir)
	table.move(writes, 1, #writes, #wall + 1, wall)
	local result = host.sh(("cd %s && TMPDIR=%s %s %s"):format(quote(dir), quote(state .. "/tmp"),
		quote(kit .. "/check"), quote(dir)),
		{ timeout = config.timeout, read = wall, write = { state .. "/tmp" }, hangup = true })
	if result.timed_out then
		return nil, ("the check timed out after %g s"):format(config.timeout)
	elseif result.code ~= 0 then
		return nil, config.blind and "the check failed" or "the check failed:\n" .. tail(result.out, 1500)
	end
	local last = result.out:match("([^\n]*)%s*$")
	local score = tonumber(last)
	if not score then
		return { kind = "seconds", value = seconds }
	end
	if not (score > 0 and score < math.huge) then
		return nil, "the check's score must be a positive, finite number" .. (config.blind and "" or ", not " .. last)
	end
	return { kind = "score", value = score }
end

-- How much better a is than b, as a fraction: a speed-up for times, a relative difference for scores.
local function gain(a: Measure, b: Measure): number
	if a.kind == "seconds" then
		return a.value > 0 and b.value / a.value - 1 or 0
	end
	return a.value / b.value - 1
end

local function median(list: { number }): number
	local sorted = table.clone(list)
	table.sort(sorted)
	local n = #sorted
	return n % 2 == 1 and sorted[(n + 1) // 2] or (sorted[n // 2] + sorted[n // 2 + 1]) / 2
end

local function percent(fraction: number): string
	return ("%+.1f%%"):format(fraction * 100)
end

local function show(kind: string, value: number): string
	return kind == "seconds" and ("%.3f s"):format(value) or ("score %.6g"):format(value)
end

-- Measures the candidate beside the champion in config.runs pairs. Returns "better", "worse" or "same", and the
-- round: the median gain and each side's values; or nil, nil and why the candidate failed. A champion that fails
-- raises: that is the kit's or the machine's fault, not the candidate's.
local function round(built: string, champion: Champion): (string?, Round?, ...string?)
	local gains: { number }, mine: { number }, theirs: { number }, kind: string? = {}, {}, {}, nil
	for _ = 1, config.runs do
		local a: Measure?, b: Measure?, why: string?, champion_why: string?
		if math.random() < 0.5 then
			a, why = measure(built)
			if not a then
				return nil, nil, why
			end
			b, champion_why = measure(champion.built)
		else
			b, champion_why = measure(champion.built)
			if b then
				a, why = measure(built)
			end
		end
		if not b then
			error("the champion failed, so the kit or the machine is not steady: " .. tostring(champion_why), 0)
		end
		if not a then
			return nil, nil, why
		end
		if a.kind ~= b.kind or (kind and a.kind ~= kind) then
			return nil, nil, "the check reported a score for some runs and not others"
		end
		kind = a.kind
		table.insert(gains, gain(a, b))
		table.insert(mine, a.value)
		table.insert(theirs, b.value)
	end
	local middle, ahead = median(gains), 0
	for _, g in gains do
		ahead += g > 0 and 1 or 0
	end
	-- config.runs is at least 1, so there was a pair, and the kind is known.
	local result: Round = { gain = middle, kind = kind :: string, mine = mine, theirs = theirs }
	if middle > config.margin and ahead >= #gains - 1 then
		return "better", result
	end
	return middle < -config.margin and "worse" or "same", result
end

-- The state of the judge -----------------------------------------------------------------------------------------

local champion: Champion? -- nil until something passes
local start_value: Measure? -- when the starting point passed
local trials, ledger: { string }, counts: { [string]: number } = 0, {}, {}
local current: { entry: Entry, dir: string }? -- the trial being judged

local function record(entry: Entry)
	local line: string = json.encode(entry)
	table.insert(ledger, line)
	host.write(state .. "/ledger.jsonl", table.concat(ledger, "\n") .. "\n")
	local verdict = entry.verdict :: string -- a trial has its verdict by the time it is recorded
	counts[verdict] = (counts[verdict] or 0) + 1
end

local function crown(trial: number, dir: string, kind: string, value: number)
	if champion then -- the tree it was built in; its patch stays
		shell("rm -rf " .. quote(champion.dir))
	end
	shell(("rm -rf %s/built/.judge && cp %s/trials/%d.patch %s/champion.patch"):format(quote(dir), quote(state), trial,
		quote(state)))
	champion = { trial = trial, dir = dir, built = dir .. "/built", kind = kind, value = value }
end

local function champion_name(c: Champion): string
	return c.trial == 0 and "the starting point" or "trial " .. c.trial
end

local function start()
	local dir = state .. "/start"
	git("init --quiet --bare " .. quote(git_dir), "")
	snapshot(dir .. "/src")
	start_tree = tree(dir .. "/src")
	write_patch(start_tree, "champion.patch") -- empty: the start is the champion until something beats it
	local ok, why = build(dir)
	local result: Measure?
	if ok then
		result, why = measure(dir .. "/built")
	end
	if result then
		champion = { trial = 0, dir = dir, built = dir .. "/built", kind = result.kind, value = result.value }
		start_value = result
		say(("the starting point passes (%s); it is the champion"):format(show(result.kind, result.value)))
	else
		say("the starting point does not pass, so the first submission that does becomes the champion:\n"
			.. tostring(why))
	end
	record({ trial = 0, verdict = result and "start" or "start fails", why = why, value = result and result.value })
end

local function judge(entry: Entry, dir: string): (string, ...Measure)
	local ok, why = build(dir)
	if not ok then
		entry.verdict, entry.why = "broken", why
		return "broken: the build failed, so nothing was measured.\n" .. tostring(why)
	end
	if not champion then
		local result: Measure?
		result, why = measure(dir .. "/built")
		if not result then
			entry.verdict, entry.why = "invalid", why
			return "invalid: nothing passes yet.\n" .. tostring(why)
		end
		entry.verdict, entry.value = "champion", result.value
		return "champion: this is the first version that passes, so it is the champion now, and later submissions "
			.. "are measured against it.", result
	end
	local verdict, screen, failed = round(dir .. "/built", champion)
	if not screen then
		entry.verdict, entry.why = "invalid", failed
		return "invalid: " .. tostring(failed)
	end
	entry.screen = screen
	local numbers = ("%s against the champion (medians %s and %s)"):format(percent(screen.gain),
		show(screen.kind, median(screen.mine)), show(screen.kind, median(screen.theirs)))
	if verdict ~= "better" then
		local named = verdict == "same" and "no different" or "worse"
		entry.verdict = named
		return config.blind and named .. ": not better than the champion by the margin."
			or ("%s: %s in %d pair%s; a change counts from %s."):format(named, numbers, config.runs,
				config.runs == 1 and "" or "s", percent(config.margin))
	end
	local confirmed, again, failed_again = round(dir .. "/built", champion)
	if not again then
		entry.verdict, entry.why = "invalid", failed_again
		return "invalid on fresh runs: " .. tostring(failed_again)
	end
	entry.confirm = again
	if confirmed ~= "better" then
		entry.verdict = "not confirmed"
		return config.blind and "not confirmed: better at first, but not on fresh runs."
			or ("not confirmed: %s at first, but %s on fresh runs."):format(numbers, percent(again.gain))
	end
	entry.verdict = "win"
	return (config.blind and "win: better than the champion, and again on fresh runs."
		or ("win: %s, and %s again on fresh runs."):format(numbers, percent(again.gain)))
		.. " This is the champion now.", { kind = again.kind, value = median(again.mine) }
end

local function submit(note: string): string
	trials += 1
	local trial = trials
	local dir = ("%s/trials/%d"):format(state, trial)
	local entry: Entry = { trial = trial, time = os.time(), note = note:sub(1, 2000) }
	current = { entry = entry, dir = dir }
	snapshot(dir .. "/src")
	write_patch(tree(dir .. "/src"), ("trials/%d.patch"):format(trial))
	local answer, crowned = judge(entry, dir)
	if crowned then
		crown(trial, dir, crowned.kind, crowned.value)
	end
	record(entry)
	current = nil
	if not crowned then
		shell("rm -rf " .. quote(dir))
		if champion then
			answer ..= ("\nThe champion is still %s."):format(champion_name(champion))
		end
	end
	say(("trial %d: %s"):format(trial, entry.verdict :: string)) -- judge gave every trial its verdict
	return ("Trial %d. %s"):format(trial, answer)
end

-- A trial the judge could not finish, through no fault of the submission, or that a stop cut short.
local function abandon(err: unknown): string
	if not current then -- the verdict is in the ledger already; what failed came after it
		return "the judge failed after recording its verdict: " .. tostring(err)
	end
	local entry, dir = current.entry, current.dir
	current = nil
	local verdict, why = (err == "hung up" or err == "terminated") and "stopped" or "error", tostring(err)
	entry.verdict, entry.why = verdict, why
	host.try(record, entry)
	host.try(host.sh, "rm -rf " .. quote(dir))
	say(("trial %d: %s: %s"):format(entry.trial, verdict, why))
	return ("Trial %d. %s: the judge could not finish it, through no fault of the submission: %s"):format(entry.trial,
		verdict, why)
end

local function summary_text(): string
	local parts = {}
	for _, verdict in { "win", "champion", "not confirmed", "no different", "worse", "invalid", "broken", "error",
		"stopped" } do
		if counts[verdict] then
			table.insert(parts, ("%d %s"):format(counts[verdict], verdict))
		end
	end
	local lines = { ("hx judge: %d trial%s%s"):format(trials, trials == 1 and "" or "s",
		#parts > 0 and ": " .. table.concat(parts, ", ") or "") }
	if champion and champion.trial > 0 then
		local against = start_value and (", the start %s"):format(show(start_value.kind, start_value.value)) or ""
		table.insert(lines, ("the champion is trial %d (%s%s); apply it with git apply %s/champion.patch"):format(
			champion.trial, show(champion.kind, champion.value), against, state))
	elseif champion then
		table.insert(lines, ("nothing beat the starting point (%s)"):format(show(champion.kind, champion.value)))
	else
		table.insert(lines, "nothing passed")
	end
	table.insert(lines, ("the ledger is %s/ledger.jsonl"):format(state))
	return table.concat(lines, "\n")
end

-- Prints the summary and leaves only the ledger and the patches: the trees were copies.
local function summary()
	host.log(summary_text())
	host.sh(("cd %s && rm -rf run tmp git index index.lock start && find trials -mindepth 1 -maxdepth 1 -type d -exec rm -rf {} +")
		:format(quote(state)))
end

local function main(): number
	local given, why = read_config()
	if not given then
		print(why)
		return 2
	end
	config = given
	math.randomseed(os.time())
	local started, err = host.try(start)
	if not started then
		print((("cannot measure the starting point: " .. tostring(err)):gsub("\n", " ")))
		return 1
	end
	print("ready")
	while true do
		local read, line = host.try(host.input)
		if not read or not line then
			break
		end
		local request = json.decode(line)
		local note = type(request) == "table" and type(request.note) == "string" and request.note or ""
		local done, answer = host.try(submit, note)
		if not done then
			local stop = answer == "hung up"
			answer = abandon(answer)
			if stop then
				break -- hx is gone: judge nothing more
			end
		end
		print(json.encode(answer))
	end
	summary()
	return 0
end

return { main = main }
