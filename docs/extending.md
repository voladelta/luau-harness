# Extending hx

The controller, prompt, printer and hooks are Luau in [`stdlib/std/`](../stdlib/std/); you can override them, and add tools, packages, skills and MCP servers. The Luau API cannot approve source, and accepted changes apply to new runs only (no hot reload).

## Extension kinds

| Kind | Where | How it becomes active |
|---|---|---|
| Module override | `.harness/override/std/<mod>.luau` | On a new run; `require("base:std/<mod>")` wraps the original. Invalid controller code stops the run |
| Local tool | `.harness/tools/NAME.lua` or `NAME.luau` | `hx trust`; loads on a new run as `NAME.method(...)` after validation and credential checks |
| Tool package | `.harness/pkgs/NAME/` with `hx.toml`, `kind = "tool"` | `hx approve NAME`; model calls `tools.NAME.fn(...)` |
| Skill | `.harness/pkgs/NAME/` with `SKILL.md`, `kind = "skill"` | `hx approve NAME`; model calls `skill("NAME")` (or finds it with `skills("words")`; skills are not listed in the prompt) |
| Agent skill | `.agents/skills/NAME/SKILL.md` or `~/.agents/skills/NAME/SKILL.md` (no `hx.toml`; project shadows global) | `hx approve NAME` (`hx skills` lists them); file tools gain read access to its directory |
| MCP server | `.harness/mcp.toml` | On the next run (see [configuration](configuration.md#mcptoml)) |
| Agent-written tool | `deftool(...)` then `promote(name)` (with `--pack std/authoring`) | Staged without execution; you review, run `hx test DIR`, accept it, and start a new run |
| Agent-written module replacement | `propose_module("std/MODULE", source, {reason})` (with `--pack std/authoring`) | Staged; `hx accept std/MODULE` shows the reason, a structural check (it loads and keeps every export) and the diff, then makes it the override. `hx rollback std/MODULE` undoes it |

A package that is unapproved, or changed since approval, is invisible to a new run. Each approved package call uses a fresh VM and dispatch-time capability checks. Imports are restricted to that package; function references and host handles cannot cross its boundary. Every module accept and rollback is appended to `.harness/upgrades.jsonl`.

Control hooks can deny or transform commands; they cannot grant authority. A controller-hook failure stops its transition; observer failures are diagnostic.

## Single-file tools

Drop a file in `.harness/tools/`, then review it with `hx trust`. Its file name is the namespace: lowercase letters, digits and underscores, starting with a letter. A file returns a definition:

```lua
-- .harness/tools/greeting.lua
return {
    tools = {
        hello = {
            signature = "hello(name: string): string",
            description = "Greet someone by name.",
            run = function(name) return "Hello, " .. name end,
        },
    },
}
```

The model can call `greeting.hello("Ada")`.

- Every method needs a signature, description and `run` function. Successful registrations contribute their signatures to the model prompt.
- Invalid definitions, missing credentials, reserved names and namespace collisions reject the whole file with a warning; other tools still load.
- Both `.lua` and `.luau` use Luau syntax. Defining the same namespace in both files rejects both.
- Registration is pure and limited to one second and 16 MiB.
- Each method call runs the frozen source in a fresh isolated VM with copied arguments and results; mutable locals do not persist between calls.
- Local tools cannot import privileged harness modules.

### Capabilities

For filesystem, shell or model access, declare only what you need:

```lua
capabilities = { fs = {"read", "write"}, proc = {"sh"}, llm = {"agent", "chat"} }
```

| Capability | Grants |
|---|---|
| `fs.read` | `read`, `search` within the run's roots |
| `fs.write` | `write`, `edit` within the run's roots |
| `proc.sh` | `sh` |
| `llm.agent` | `agent(task)` |
| `llm.chat` | `llm{{system=..., prompt=..., effort=..., max_tokens=...}, ...}`: up to 32 single-shot requests sent concurrently to the run's provider and model, budgeted and journaled like any model call. `effort` defaults to the run's; `"none"` sends no reasoning setting. Returns `{ok, value}` or `{ok, error}` per request in input order. Needs `hx run` |

Every tool also gets `nonce()`, 32 URL-safe characters of fresh OS entropy. Workspace trust grants these declarations; the file-tool path, budget and approval checks still apply.

### HTTP endpoints

HTTP tools declare named, fixed endpoints in `http`, then call `http("endpoint", args)` inside a method.

| Field | Default | Meaning |
|---|---|---|
| `url` | | Fixed origin and path; a `{name}` placeholder fills one whole path segment (`/videos/{job_id}`, not `/v{id}`) from the argument of that name, percent-encoded |
| `method` | `GET` | Remaining GET arguments become encoded query parameters; other methods send JSON |
| `secret` | | Credential name |
| `header` | `Authorization` | Header the secret is sent in, verbatim |
| `bearer` | `false` | Send `Bearer <secret>` (store the bare token) |
| `raw` | `false` | Return `{text, media_type}` for UTF-8 bodies or `{base64, media_type}` for audio, image, video and other bytes, instead of decoded JSON |
| `read_only` | `false` | Mark only endpoints that read data; others go through the approval gate, matched as `http NAMESPACE.ENDPOINT` |

Tool code also gets `json.encode`, `json.decode` and `base64.decode`, whose bytes can go straight to `write`.

- HTTP errors raise with their status and a bounded response excerpt.
- Requests have a 120-second timeout, an 8 MiB response limit, no retries and no redirects.
- Endpoint declarations grant access to those URLs; the environment running hx controls network access.
- HTTP calls use the supervisor's authorization, run deadline and journal.
- Credentials are attached by Rust and never returned to tool code or saved in the run manifest. Credentials echoed in responses are redacted before journaling.

### Credentials

Credential lookup checks, in order:

1. A nonblank environment variable.
2. `.harness/secrets.toml` in the workspace.
3. `$XDG_CONFIG_HOME/hx/secrets.toml`, or `~/.config/hx/secrets.toml`.

Secrets files contain top-level string assignments, for example `TINYFISH_API_KEY = "your-key"`. Blank values fall through to the next source. Without a usable key, the tool's whole registration fails and its methods stay unavailable.

Secrets files are blocked from file-tool reads and searches; the external sandbox must restrict shell access. Keep them out of version control. Keys can rotate without changing workspace trust; they are resolved again at registration and request time, including on resume.

## Included example tools

Each one installs the same way:

```bash
mkdir -p .harness/tools
cp examples/tools/NAME.lua .harness/tools/NAME.lua
hx trust
```

| Tool | Needs | Provides |
|---|---|---|
| [`tinyfish`](../examples/tools/tinyfish.lua) | `TINYFISH_API_KEY` | Web search and fetch |
| [`openrouter`](../examples/tools/openrouter.lua) | `OPENROUTER_API_KEY` | Model discovery, credits, text/image/audio/video generation |
| [`ssot`](../examples/tools/ssot.lua) | nothing (uses the run's model) | Deliberately diverse candidate answers |

### TinyFish search and fetch

```bash
export TINYFISH_API_KEY='your-key'
hx exec 'print(tinyfish.search{query="Luau embedding", include_domains="luau.org"})'
hx exec 'local r=tinyfish.fetch{urls={"https://luau.org"}}; print(r.results, r.errors)'
```

The adapter uses TinyFish's documented [Search API](https://docs.tinyfish.ai/search-api/reference) and [Fetch API](https://docs.tinyfish.ai/fetch-api/reference), preserves their response objects, and supports the documented filter and extraction options. Fetch accepts 1–10 URLs; check `errors` even when some results succeed.

### OpenRouter generation and model discovery

```bash
export OPENROUTER_API_KEY='your-key'
hx exec 'print(openrouter.credits())'
hx exec 'print(openrouter.help{model="qwen/qwen-image-2.1-turbo"})'
hx exec 'print(openrouter.models{output_modalities="video"})'
hx exec 'print(openrouter.text{model="deepseek/deepseek-chat-v3.1", prompt="Hello OpenRouter!"})'
```

Registration requires `OPENROUTER_API_KEY` in the environment or a secrets file; without it, `openrouter` is `nil`.

| Method | What it does |
|---|---|
| `openrouter.credits()` | Queries `/api/v1/credits`; returns `{ total_credits, total_usage, remaining_credits }` |
| `openrouter.models{...}` | Discovers, filters and searches the 400+ models from `/api/v1/models`; caches the catalog for 24 hours (`force=true` bypasses) |
| `openrouter.help{model="..."}` | Fetches the model guide (`https://openrouter.ai/{provider}/{model-id}/llms.txt`) and root docs (`https://openrouter.ai/docs/llms.txt`); 24-hour cache (`force=true` bypasses) |
| `openrouter.text{model, prompt\|messages, ...}` | Text generation via `/api/v1/responses`; optional `max_output_tokens` and `text` configure the output. Returns `{id, text, usage, model, raw}` |
| `openrouter.image{model, prompt, ...}` | Text-to-image; `save_to` writes every image (`name.png`, `name-2.png`, ...), returns the paths as a list and drops the base64. A failed write raises |
| `openrouter.sound{model, input, ...}` | Text-to-speech; `save_to` writes the audio and omits the base64. A failed write raises |
| `openrouter.video{model, prompt, ...}`, `openrouter.poll_video{job_id}` | Video generation submission and job polling |

### SSoT: diverse candidates from the run's model

Based on [String Seed of Thought](https://pub.sakana.ai/ssot/). `ssot.sample{prompt, n=5}` asks the run's model for `n` deliberately different answers in parallel and returns `{answers = {{answer, seed}, ...}, errors}`.

Each request gets its own `nonce()` seed and is told to list the root-level dimensions along which answers can differ, pick one option per dimension from a different part of the seed, then answer. Reasoning is off unless `effort` says otherwise.

Use it for open-ended work (ideas, designs, angles, drafts), then judge the candidates: some are deliberately unusual and some are weak.

In blind tests on DeepSeek V4.1 Flash (10 samples per arm; UI layouts, blog posts and trading research ideas), the paper's plain prompt barely moved answers off the model's default. This seeded-dimensions prompt gave 10 of 10 distinct blog posts against 0 of 10, and raised the novelty of trading ideas from 2.3 to 3.25 out of 5, at some cost in soundness (3.5 to 3.15).
