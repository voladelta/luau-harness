# Configuration

All configuration lives in `.harness/` in the workspace. The model can read it but never write it. Workspace config is used only after you trust it with `hx trust` (see [Workspace trust](#workspace-trust)).

| File | Purpose |
|---|---|
| `.harness/policy.toml` | Sandbox roots, network, GPU, approval gate, budgets, packs |
| `.harness/mcp.toml` | MCP servers |
| `.harness/init.luau` | Toggles for `std` modules such as compaction and recheck |
| `.harness/ar.toml` | Autoresearch setup (see [workflows](workflows.md#harnessartoml)) |
| `.harness/override/`, `.harness/tools/`, `.harness/pkgs/` | Extensions (see [Extending](extending.md)) |
| `.harness/secrets.toml` | Credentials for tools (see [credentials](extending.md#credentials)) |
| `hx.lock` | Approved packages and their content hashes |

## `policy.toml`

```toml
[fs]
read = ["/extra/read/root"]     # extra read roots for the sandbox
write = ["/extra/write/root"]   # extra write roots

[net]
allow = false                   # all-or-nothing; Seatbelt cannot filter by host

[gpu]
allow = false                   # true lets `sh` use Metal (Skia/wgpu renderers such as fframes)

[approval]
mode = "deny"                   # "deny" (headless default) or "prompt"
ask = ["terraform apply"]       # extra command substrings that need approval
allow = ["git push origin"]     # pre-approved (substring; irreversible commands need an exact match)

[budget]
max_tokens = 600000
max_wall_secs = 900

[packs]
enable = []
```

Budgets are admission thresholds: each request reserves its upper bound in tokens (request bytes plus the output cap), and the deadline is absolute, so downtime counts. There is no dollar budget; an unknown key such as `max_cost` is an error. `hx run` flags override these values for a new run.

## `mcp.toml`

```toml
[server.notes]                  # local server, runs under its own Seatbelt profile
command = "python3"
args = ["server.py"]
read_only_tools = ["read_*"]    # these run freely; every other tool needs approval
env = ["NOTES_DIR"]             # the only env vars passed through
net = false

[server.remote]                 # remote Streamable HTTP server (https, or http on localhost)
url = "https://mcp.example.com/mcp"
bearer_env = "REMOTE_TOKEN"     # or headers_env = {"x-api-key" = "VAR"}
```

MCP tools bind as `mcp.SERVER.tool{...}`. Approve a non-read-only tool with `allow = ["mcp notes.add"]` in `policy.toml`.

## `init.luau`: compaction and recheck

**Compaction** (`std/compact`) is on by default. From 256k tokens at the latest (earlier for a smaller `--context-window`), older history becomes a structured summary; the task message, recent messages and Luau globals stay. The summarizer call replays the agent's own request prefix, so it sees exact messages and can hit the provider's prompt cache. A summary that is truncated, calls a tool or is not smaller than what it replaces is rejected and the history is kept. On a context-overflow error hx treats the rejected size as an upper bound on the window, compacts once keeping as little recent history as fits under it, and retries. Disable it for new runs:

```luau
require("std/compact").enabled = false
```

**Recheck** (`std/recheck`) is opt-in. The system prompt warns that the first final answer will be checked; before it is accepted, the model is sent back once to name the root cause, list the other cases it implies, test each and re-check the task's requirements. It adds turns and can hurt build tasks; enable it for debugging tasks:

```luau
require("std/recheck").enabled = true
```

## Workspace trust

`.harness/{policy,mcp}.toml`, `.harness/init.luau`, `.harness/override/`, `.harness/tools/` and `hx.lock` live in the repo, so whoever wrote the repo could widen the sandbox, start MCP commands or pre-approve packages and skills. hx ignores that config until you review it with `hx trust`, which records its hash in `~/.config/hx/trust.toml` (outside the repo, protected from the model).

- Any edit makes the workspace untrusted again.
- `hx approve`, `hx accept` and `hx rollback` keep an already trusted workspace trusted.
- A workspace without such files needs nothing.
- `HX_TRUST=1` in hx's environment trusts the workspace for one launch (tests, throwaway copies).

## Approvals and `hx.lock`

`hx.lock` is the single approval record: per package, the approved version and its content hash. The hash covers the package's `hx.toml`, so approving a package grants exactly the capabilities its manifest requests, and a manifest that asks for more no longer matches its approval.

Older lock files may still carry `[package.grants]` tables or a `.harness/grants.toml`; hx ignores both.
