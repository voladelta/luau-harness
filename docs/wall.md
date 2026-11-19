# The wall

| | Allowed |
|---|---|
| Write | the project directory (where you start `hx`), a private `$TMPDIR` that is removed on exit, `--rw` paths |
| Read | the above, plus `/usr /bin /sbin /lib* /etc /opt /proc /sys /nix/store`, the directories on your `PATH` (below), your git configuration (below), trust roots (below), `--ro` paths |
| Devices | `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom` |
| Everything else | cannot be opened: the rest of your home directory, other terminals, other projects |

A program on your `PATH` runs inside, wherever it is installed. Each absolute directory on `PATH` is readable,
and so is the directory that each symbolic link in it points into, with that directory's `lib`, `lib64` and
`libexec` beside it when it is a `bin`: `~/.bun/bin/bun` runs, and `~/.local/bin/python3` linked into a uv-managed
Python finds its library. Nothing next to them opens: not `~/.bun` or `~/.local`, nor anything else in the home
directory. A `PATH` entry that is `/`, the home directory or a directory that holds it is left out, as are
relative entries. To run with a narrower wall, narrow `PATH`: `PATH=/usr/bin:/bin hx "..."`.

What a toolchain needs beyond its programs takes a flag, for example
`hx --ro ~/.rustup --rw ~/.cargo/registry "..."` for rustup's toolchains and cargo's downloads. The flags and
`PATH` come from your command line and environment on purpose: nothing inside the project, which the model can
write, can widen the wall.

Git reads your name, aliases and ignore rules inside: `~/.gitconfig`, and `config`, `ignore` and `attributes` in
`$XDG_CONFIG_HOME/git` (`~/.config/git` by default). Nothing else in that directory opens, so the tokens
`git credential-store` keeps in `credentials` beside them stay outside. A token written into the configuration
itself, in a URL or an `http.extraHeader`, is readable with it: keep those in a credential helper.

Behind a TLS-intercepting proxy the trust roots often live in your home directory. The file or directory named
by each of `SSL_CERT_FILE`, `SSL_CERT_DIR`, `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`,
`GIT_SSL_CAINFO`, `CARGO_HTTP_CAINFO`, `AWS_CA_BUNDLE` and `NIX_SSL_CERT_FILE`, when it is an absolute path, is
readable inside, so `curl`, `git`, `pip` and `npm` started by the model trust what you trust. These come from your
environment, which the model cannot change. `hx`'s own request uses `SSL_CERT_FILE`, or else `CURL_CA_BUNDLE`.

Also in force:

- `hx` refuses to start in `/`, in your home directory or in any directory that contains it, such as `/home`,
  where the wall would leave your home directory open. It refuses a `--ro` or `--rw` path of that kind too.
- No setuid program can raise privileges (`no_new_privs`), so `sudo` does not work inside.
- On Linux 6.12 or later, confined processes cannot signal processes outside the sandbox or reach their
  abstract unix sockets.
- Unix sockets by path are walled off too. From Landlock ABI 9 the kernel refuses a connection to a socket whose
  path the rules above do not make readable. Before ABI 9 Landlock cannot, so on x86-64 and aarch64 a seccomp
  filter refuses to create unix sockets (a stream `socketpair` still works, a datagram one does not, because
  it could still send to any path; `hx` itself falls back where it would use a unix socket, such as for name
  resolution). The filter also refuses `io_uring`, which can create sockets
  without the socket call, and every call from 32-bit code. That breaks tools that open unix sockets by path:
  the docker CLI, tmux, some test suites; and 32-bit programs. `--unix-sockets` leaves the filter out, and `hx`
  then prints a note at startup.
- The API key is read once and removed from the environment, together with every variable whose name ends in
  `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD` or `CREDENTIALS` (`MY_API_KEY`, `OPENAI_APIKEY`, `PGPASSWORD`),
  or in `_PAT` or `_PWD` (`GITHUB_PAT`, `MYSQL_PWD`; `PWD` and `OLDPWD` stay). `--keep-env NAME` keeps one. Luau
  code never sees the key: the host adds it to requests for the configured endpoint only, and commands cannot
  read it out of the host's memory. `ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY` and `EXA_API_KEY` are kept the same way,
  whatever the model, and each is sent only to its own endpoints: `HX_ANTHROPIC_URL`; `HX_DEEPSEEK_URL` and
  DeepSeek's tools, `HX_TOOLS_URL/search`, `/fetch` and `/ocr`; and `HX_EXA_URL`.
- The VM has a memory ceiling of 1 GiB, and below it takes only what the system can spare: each time it grows by
  64 MiB it reads `MemAvailable` in `/proc/meminfo` again, and it refuses to grow into the last 128 MiB, so that
  it runs out before the kernel's out-of-memory killer acts. Code that runs into either gets the error "not
  enough memory", and what it held is collected at the next step, so the session goes on with its memory back.
- Every process a command started is killed when `hx` exits, a daemon that called `setsid` included: `hx` is a
  child subreaper, so all of them stay its descendants.

## What the wall does not do

Know these before you rely on it.

- **With `--unix-sockets` on a kernel before Landlock ABI 9, or on another architecture than x86-64 or
  aarch64, unix sockets by path are reachable.** A confined command cannot list `/run/user/1000`, but it can
  connect to a socket there. That puts the D-Bus session bus and systemd's user socket (both can start programs
  outside the sandbox), `ssh-agent`, Wayland and a Docker socket within reach. `hx` prints a note at startup when
  this is so. To close it then, run `hx` as a separate Unix user that owns nothing but the project.
- **The network is open.** The model can send anywhere what it can read. The wall limits what it can read.
- **What the model writes, you may later run.** A changed `Makefile`, test, `.git/hooks` script or `.git/config`
  runs with your full access when you use it outside `hx`. Hooks and `.git/config` do not show in `git diff`,
  and a `core.fsmonitor` or pager set there runs on your next `git status`, the very command you would review
  with. When the task reads text you do not trust, run `hx` in a clone (`git clone project project-hx`) and
  `git pull` from it: commits cross over, hooks and configuration do not.
- **Secrets stored in the project are readable**, as are secrets in variables with unusual names.
- **A process outlives `hx` when `hx` itself is killed with SIGKILL, or crashes.** Every other way out kills
  what the model started, leaving at once on a second Ctrl-C included. Such a process stays confined, but it is
  not killed. A process stuck in uninterruptible I/O (a dead network mount) is sent SIGKILL but dies only when
  the I/O returns; `hx` waits a second for it, then leaves without it. A command that times out is treated the
  same way: `host.sh` returns after that second, with `signal` 9.
- **The model can edit `.hx/loop.luau` and its extensions.** That is the point, and it shows in `git diff`.
  A loop that no longer loads, or returns no `main` function, stops hx rather than running a session without
  what the project put there; `hx --built-in-loop` runs without it, to repair it say.
- **Nothing asks before a file in the project is changed or deleted.** Git is the undo.
