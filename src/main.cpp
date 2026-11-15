// hx: a small host for a per-project coding agent.
//
// The host is a Luau VM plus a handful of primitives (model call, shell, files). The agent loop itself is a
// Luau file the project owns. Before any of that runs, the process confines itself with Landlock, so the VM,
// the file primitives and every command the model starts share one set of walls.

#include <curl/curl.h>
#include <fcntl.h>
#include <sys/prctl.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cctype>
#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <exception>
#include <filesystem>
#include <fstream>
#include <initializer_list>
#include <iostream>
#include <iterator>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <fmt/format.h>

#include "embedded.h"
#include "host.h"
#include "json.h"
#include "judge.h"
#include "lua.h"
#include "luacode.h"
#include "lualib.h"
#include "sandbox.h"
#include "sys.h"
#include "usage.h"

namespace {

namespace fs = std::filesystem;

constexpr std::string_view kUsage = R"(hx: a coding agent for the project in the current directory, confined by Landlock

usage:
  hx [options] [TASK...]              run the agent on TASK; with no task, read it from stdin, or
                                      start a prompt when stdin is a terminal
  hx [options] --run FILE [ARGS...]   run a Luau script with the host primitives (FILE - reads stdin);
                                      every word after FILE goes to the script, even one starting with -
  hx --init [--full]                  write .hx/loop.luau, a loop that starts from the built-in one;
                                      with --full, a copy of the whole built-in loop
  hx --judge KIT [options] [TASK...]  run the agent with a judge: only changes it submits and the kit's
                                      measurements confirm count; KIT must lie outside the wall

examples:
  hx "add a --json flag to the export command and test it"
  git diff | hx review this change
  hx --ro ~/.rustup --ro ~/.cargo --rw ~/.cargo/registry "fix the failing test"
  hx -q "summarise the README" > summary.txt

options:
  --model NAME      model to ask (overrides HX_MODEL): opus, sonnet, haiku or a full name
  --judge KIT       judge the agent's changes with the kit in directory KIT (run, and optionally build
                    and check); the agent cannot read the kit, and the champion, its patch and the
                    ledger go to a directory outside the wall
  --ro PATH         also allow reading PATH (a toolchain in your home directory, for example)
  --rw PATH         also allow writing PATH
  --keep-env NAME   keep a variable that looks like a secret (its name ends in KEY, TOKEN, SECRET,
                    PASSWORD, PASSWD or CREDENTIALS) in the environment of commands
  --unix-sockets    before Landlock ABI 9, let commands create unix sockets; they can then connect to
                    any socket by path, the D-Bus session bus and ssh-agent included
  --no-sandbox      run without Landlock (the model can then touch anything you can)
  -q, --quiet       leave out progress lines on stderr; the answer and errors still show
  --no-input        never prompt; with no task and stdin a terminal, fail instead
  -h, --help        show this text
  --version         show the version

  Options go before the task: every word from the first one that is not an option on is the task.
  A value can follow its option as the next word or after =, as in --ro=PATH.

environment:
  HX_MODEL          model name; opus, sonnet and haiku are the current Claude models (default
                    deepseek-flash, or claude-opus-5-5 when only ANTHROPIC_API_KEY is set)
  HX_API_URL        Anthropic Messages endpoint (default https://api.anthropic.com/v1/messages for
                    a claude-* model, https://api.deepseek.com/anthropic/v1/messages otherwise)
  HX_KEY_ENV        name of the variable that holds the API key (default ANTHROPIC_API_KEY for a
                    claude-* model, DEEPSEEK_API_KEY otherwise)
  HX_EFFORT         how hard the model thinks: low, medium, high, xhigh or max (default: medium on
                    the Claude models hx knows, the endpoint's own otherwise)
  HX_TOOLS_URL      base of DeepSeek's search, fetch and OCR endpoints, which the built-in web and ocr
                    extensions use with DEEPSEEK_API_KEY, whatever the model (default
                    https://api.deepseek.com/v1/tools)
  HX_EXA_URL        Exa's search endpoint, which the built-in exa extension uses for deep search with
                    EXA_API_KEY, whatever the model (default https://api.exa.ai/search)

output:
  The model's words go to stdout; progress, warnings and errors go to stderr.

exit status:
  0 done, 1 the run failed, 2 bad usage or hx cannot start here, 130 interrupted, 143 terminated

docs: https://github.com/voladelta/luau-harness
)";

constexpr const char* kDeepSeekUrl = "https://api.deepseek.com/anthropic/v1/messages";
constexpr const char* kDeepSeekKeyEnv = "DEEPSEEK_API_KEY";
constexpr const char* kDeepSeekModel = "deepseek-flash";
constexpr const char* kDeepSeekToolsUrl = "https://api.deepseek.com/v1/tools";
constexpr const char* kExaKeyEnv = "EXA_API_KEY";
constexpr const char* kExaUrl = "https://api.exa.ai/search";
constexpr const char* kAnthropicUrl = "https://api.anthropic.com/v1/messages";
constexpr const char* kAnthropicKeyEnv = "ANTHROPIC_API_KEY";
constexpr const char* kAnthropicModel = "claude-opus-5-5";

// Short names for the current Claude models.
constexpr std::pair<std::string_view, std::string_view> kModelAliases[] = {
    {"opus", "claude-opus-5-5"},
    {"sonnet", "claude-sonnet-5-5"},
    {"haiku", "claude-haiku-5-5"},
};
constexpr const char* kProjectLoop = ".hx/loop.luau";

constexpr int kExitFailure = 1;
constexpr int kExitUsage = 2;
constexpr int kExitInterrupted = 130;  // 128 + SIGINT, as a shell reports it
constexpr int kExitTerminated = 143;   // 128 + SIGTERM

using hx::UsageError;

enum class Mode : std::uint8_t { Agent, Script, Init, Help, Version };

struct Options {
    Mode mode = Mode::Agent;
    std::string script;  // --run FILE; "-" is stdin
    std::vector<std::string> args;
    std::vector<std::string> read_paths;
    std::vector<std::string> write_paths;
    std::vector<std::string> keep_env;
    std::optional<std::string> model;
    std::optional<std::string> judge;  // --judge KIT
    bool sandbox = true;
    bool unix_sockets = false;
    bool full = false;  // --init --full: copy the whole built-in loop, not the overlay
    bool quiet = false;
    bool no_input = false;
};

// Every option hx knows, for the suggestion after a misspelt one.
constexpr std::string_view kOptionNames[] = {
    "--model",    "--ro",   "--rw",   "--keep-env", "--unix-sockets", "--no-sandbox", "--quiet",
    "--no-input", "--init", "--full", "--run",      "--help",         "--version",    "--judge",
};

// Edit distance, for "did you mean".
std::size_t distance(std::string_view a, std::string_view b) {
    std::vector<std::size_t> row(b.size() + 1);
    for (std::size_t j = 0; j <= b.size(); ++j) {
        row[j] = j;
    }
    for (std::size_t i = 1; i <= a.size(); ++i) {
        std::size_t diagonal = row[0];
        row[0] = i;
        for (std::size_t j = 1; j <= b.size(); ++j) {
            const std::size_t above = row[j];
            row[j] = std::min({row[j] + 1, row[j - 1] + 1, diagonal + (a[i - 1] == b[j - 1] ? 0 : 1)});
            diagonal = above;
        }
    }
    return row[b.size()];
}

UsageError unknown_option(std::string_view arg) {
    std::string_view best;
    std::size_t best_distance = 3;  // further than two edits away is not a typo
    for (const std::string_view name : kOptionNames) {
        if (const std::size_t d = distance(arg, name); d < best_distance) {
            best = name;
            best_distance = d;
        }
    }
    std::string message = fmt::format("unknown option {}", arg);
    if (!best.empty()) {
        message += fmt::format("; did you mean {}?", best);
    }
    message += "\nRun 'hx --help' for the options. To give a task that starts with -, put -- before it.";
    return UsageError{message};
}

Options parse_args(const std::vector<std::string_view>& argv) {
    Options options;
    std::size_t i = 0;
    for (; i < argv.size(); ++i) {
        const std::string_view word = argv[i];
        if (word == "--") {
            ++i;
            break;
        }
        if (word.size() < 2 || !word.starts_with('-')) {
            break;  // the task starts here
        }
        // --name=value is the same as --name value.
        std::string_view arg = word;
        std::optional<std::string_view> inline_value;
        if (const std::size_t equals = word.find('='); word.starts_with("--") && equals != std::string_view::npos) {
            arg = word.substr(0, equals);
            inline_value = word.substr(equals + 1);
        }
        const auto value = [&](std::string_view metavar) {
            if (inline_value ? inline_value->empty() : i + 1 >= argv.size()) {
                throw UsageError(fmt::format("{} needs a value: {} {}", arg, arg, metavar));
            }
            return std::string(inline_value ? *inline_value : argv[++i]);
        };
        const auto flag = [&] {
            if (inline_value) {
                throw UsageError(fmt::format("{} takes no value", arg));
            }
        };
        if (arg == "--ro") {
            options.read_paths.push_back(value("PATH"));
        } else if (arg == "--rw") {
            options.write_paths.push_back(value("PATH"));
        } else if (arg == "--keep-env") {
            options.keep_env.push_back(value("NAME"));
        } else if (arg == "--model") {
            options.model = value("NAME");
        } else if (arg == "--judge") {
            options.judge = value("KIT");
        } else if (arg == "--unix-sockets") {
            flag();
            options.unix_sockets = true;
        } else if (arg == "--no-sandbox") {
            flag();
            options.sandbox = false;
        } else if (arg == "-q" || arg == "--quiet") {
            flag();
            options.quiet = true;
        } else if (arg == "--no-input") {
            flag();
            options.no_input = true;
        } else if (arg == "-h" || arg == "--help") {
            options.mode = Mode::Help;
            return options;
        } else if (arg == "--version") {
            options.mode = Mode::Version;
            return options;
        } else if (arg == "--init") {
            flag();
            if (options.mode == Mode::Script) {
                throw UsageError("--run and --init do not go together");
            }
            options.mode = Mode::Init;
        } else if (arg == "--full") {
            flag();
            options.full = true;
        } else if (arg == "--run") {
            if (options.mode == Mode::Init) {
                throw UsageError("--run and --init do not go together");
            }
            options.mode = Mode::Script;
            options.script = value("FILE");
            ++i;
            break;  // the rest belongs to the script
        } else {
            throw unknown_option(arg);
        }
    }
    options.args.assign(argv.begin() + static_cast<std::ptrdiff_t>(i), argv.end());
    if (options.full && options.mode != Mode::Init) {
        throw UsageError("--full goes with --init");
    }
    if (options.mode == Mode::Init && !options.args.empty()) {
        throw UsageError("--init takes no task");
    }
    if (options.judge && !options.sandbox) {
        throw UsageError("--judge needs the wall: without it the agent could read the kit and rewrite the ledger");
    }
    return options;
}

// The whole file, or the error that kept it from being read.
std::string read_file(const fs::path& path, std::error_code& ec) {
    ec.clear();
    const hx::UniqueFd fd(open(path.c_str(), O_RDONLY | O_CLOEXEC));
    if (!fd.valid()) {
        ec.assign(errno, std::generic_category());
        return {};
    }
    std::string text;
    std::array<char, 65536> buffer{};
    for (;;) {
        const ssize_t n = read(fd.get(), buffer.data(), buffer.size());
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n < 0) {
            ec.assign(errno, std::generic_category());
            return {};
        }
        if (n == 0) {
            return text;
        }
        text.append(buffer.data(), static_cast<std::size_t>(n));
    }
}

// Writes .hx/loop.luau: by default the overlay, which loads the built-in loop and changes parts of it, so fixes
// to hx still reach the project; with `full`, a copy of the whole built-in loop, which the project then owns.
int init_project(bool full) {
    if (fs::exists(kProjectLoop)) {  // before anything is made
        throw UsageError(fmt::format("{} already exists; it is yours, so it stays", kProjectLoop));
    }
    std::error_code ec;
    fs::create_directories(".hx/ext", ec);
    if (ec) {
        throw UsageError(fmt::format(".hx/ext: {}", ec.message()));
    }
    std::ofstream out(kProjectLoop, std::ios::binary);
    out << (full ? hx::kDefaultLoop : hx::kLoopOverlay);
    out.close();
    if (!out) {
        throw UsageError(fmt::format("cannot write {}", kProjectLoop));
    }
    if (full) {
        fmt::print("wrote {}: a copy of the whole built-in loop, which is this project's harness now\n", kProjectLoop);
    } else {
        fmt::print("wrote {}: it starts from the built-in loop; change there what this project needs\n", kProjectLoop);
    }
    return 0;
}

// A variable whose name ends in one of these words is treated as a secret. Matching the end, not any part, keeps
// plumbing such as GIT_CONFIG_KEY_0 or AWS_ACCESS_KEY_ID in place.
bool looks_secret(std::string name) {
    std::transform(name.begin(), name.end(), name.begin(),
                   [](unsigned char c) { return static_cast<char>(std::toupper(c)); });
    for (const std::string_view mark : {"KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIALS"}) {
        if (!name.ends_with(mark)) {
            continue;
        }
        if (name.size() == mark.size() || name[name.size() - mark.size() - 1] == '_') {
            return true;
        }
    }
    return false;
}

struct Secrets {
    std::string key;        // the model endpoint's
    std::string tools_key;  // DEEPSEEK_API_KEY, for DeepSeek's tool endpoints whatever the model
    std::string exa_key;    // EXA_API_KEY, for Exa's search endpoint whatever the model
};

// Returns the API key, the DeepSeek key and the Exa key, and removes all three, and every other variable that looks
// like a secret, from the environment. Commands the model runs inherit the environment, and with the network open an
// inherited secret is a leaked one. The entries are also wiped where they sit, because /proc/PID/environ shows the
// original memory.
Secrets take_secrets(std::string_view key_env, const std::vector<std::string>& keep) {
    Secrets secrets;
    std::vector<std::pair<std::string, char*>> doomed;
    // NOLINTNEXTLINE(cppcoreguidelines-pro-bounds-pointer-arithmetic): environ is a null-terminated C array
    for (char** entry = environ; *entry != nullptr; ++entry) {
        const std::string_view text(*entry);
        const std::size_t equals = text.find('=');
        if (equals == std::string_view::npos) {
            continue;
        }
        const std::string name(text.substr(0, equals));
        const bool is_key = name == key_env;
        if (is_key) {
            secrets.key = text.substr(equals + 1);
        }
        const bool is_tools_key = name == kDeepSeekKeyEnv;
        if (is_tools_key) {
            secrets.tools_key = text.substr(equals + 1);
        }
        const bool is_exa_key = name == kExaKeyEnv;
        if (is_exa_key) {
            secrets.exa_key = text.substr(equals + 1);
        }
        const bool kept = std::find(keep.begin(), keep.end(), name) != keep.end();
        if (is_key || is_tools_key || is_exa_key || (looks_secret(name) && !kept)) {
            doomed.emplace_back(name, *entry);
        }
    }
    for (const auto& [name, entry] : doomed) {
        unsetenv(name.c_str());  // NOLINT(concurrency-mt-unsafe): no other thread exists yet
        std::memset(entry, 0, std::strlen(entry));
    }
    return secrets;
}

std::string env_or(const char* name, std::string_view fallback) {
    const char* value = std::getenv(name);  // NOLINT(concurrency-mt-unsafe): no other thread exists yet
    return std::string(value != nullptr && *value != '\0' ? value : fallback);
}

struct ModelChoice {
    std::string model;
    std::string url;
    std::string key_env;
};

// The model, and where to send it. --model wins over HX_MODEL, and opus, sonnet and haiku name the current Claude
// models. Without either, a Claude model is asked when ANTHROPIC_API_KEY is set and DEEPSEEK_API_KEY is not, and
// DeepSeek's otherwise. A claude-* model goes to the Claude API with ANTHROPIC_API_KEY, any other to DeepSeek with
// DEEPSEEK_API_KEY; HX_API_URL and HX_KEY_ENV override either.
ModelChoice choose_endpoint(const std::optional<std::string>& flag) {
    std::string model = flag ? *flag : env_or("HX_MODEL", "");
    for (const auto& [alias, name] : kModelAliases) {
        if (model == alias) {
            model = name;
        }
    }
    if (model.empty()) {
        const bool only_anthropic = env_or(kDeepSeekKeyEnv, "").empty() && !env_or(kAnthropicKeyEnv, "").empty();
        model = only_anthropic && env_or("HX_API_URL", "").empty() ? kAnthropicModel : kDeepSeekModel;
    }
    const bool claude = model.starts_with("claude-");
    return {
        .model = model,
        .url = env_or("HX_API_URL", claude ? kAnthropicUrl : kDeepSeekUrl),
        .key_env = env_or("HX_KEY_ENV", claude ? kAnthropicKeyEnv : kDeepSeekKeyEnv),
    };
}

// A private temporary directory, exported as TMPDIR and removed with everything in it at exit.
class TempDir {
  public:
    TempDir() {
        std::error_code ec;
        std::string pattern = (fs::temp_directory_path(ec) / "hx-XXXXXX").string();
        if (ec || mkdtemp(pattern.data()) == nullptr) {
            throw UsageError(fmt::format("cannot create a temporary directory: {}", hx::error_text()));
        }
        path_ = std::move(pattern);
        setenv("TMPDIR", path_.c_str(), 1);  // NOLINT(concurrency-mt-unsafe): no other thread exists yet
    }
    TempDir(const TempDir&) = delete;
    TempDir& operator=(const TempDir&) = delete;
    ~TempDir() {
        std::error_code ec;
        fs::remove_all(path_, ec);
    }

    [[nodiscard]] const fs::path& path() const noexcept { return path_; }

  private:
    fs::path path_;
};

// libcurl's process-wide setup and teardown.
class CurlGlobal {
  public:
    CurlGlobal() {
        if (curl_global_init(CURL_GLOBAL_DEFAULT) != CURLE_OK) {
            throw std::runtime_error("cannot initialise libcurl");
        }
    }
    CurlGlobal(const CurlGlobal&) = delete;
    CurlGlobal& operator=(const CurlGlobal&) = delete;
    ~CurlGlobal() { curl_global_cleanup(); }
};

// What every wall lets in: system directories to read, harmless devices, git's configuration and trust roots.
std::vector<hx::Rule> system_rules() {
    using hx::Access;
    std::vector<hx::Rule> rules;
    for (const char* path : {"/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/opt", "/proc",
                             "/sys", "/nix/store", "/run/systemd/resolve"}) {
        rules.push_back({path, Access::Read, false});
    }
    // Of /dev only the harmless nodes. Terminals stay out: a command that could open /dev/pts could read what is
    // typed into the user's other terminals and write to them.
    for (const char* path : {"/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom"}) {
        rules.push_back({path, Access::Device, false});
    }
    // git refuses to run when it cannot read its configuration, and git is how the model's work is undone.
    if (const std::string home = env_or("HOME", ""); home.starts_with('/')) {
        rules.push_back({home + "/.gitconfig", Access::Read, false});
        rules.push_back({home + "/.config/git", Access::Read, false});
    }
    // Trust roots, often a proxy's and often in the home directory, so that hx's own requests and the curl, git,
    // pip or npm the model runs can verify TLS. These variables are the user's statement of where the roots live,
    // and nothing inside the wall can change them.
    for (const char* name :
         {"SSL_CERT_FILE", "SSL_CERT_DIR", "CURL_CA_BUNDLE", "REQUESTS_CA_BUNDLE", "NODE_EXTRA_CA_CERTS",
          "GIT_SSL_CAINFO", "CARGO_HTTP_CAINFO", "AWS_CA_BUNDLE", "NIX_SSL_CERT_FILE"}) {
        if (std::string path = env_or(name, ""); path.starts_with('/')) {
            rules.push_back({std::move(path), Access::Read, false});
        }
    }
    return rules;
}

// The --ro and --rw paths.
std::vector<hx::Rule> user_rules(const Options& options) {
    std::vector<hx::Rule> rules;
    rules.reserve(options.read_paths.size() + options.write_paths.size());
    for (const std::string& path : options.read_paths) {
        rules.push_back({path, hx::Access::Read, true});
    }
    for (const std::string& path : options.write_paths) {
        rules.push_back({path, hx::Access::Write, true});
    }
    return rules;
}

// The walls: system directories are readable, the project and a private temporary directory are writable, and
// the rest of the machine (the home directory above all) cannot be opened at all.
std::vector<hx::Rule> sandbox_rules(const Options& options, const fs::path& project, const fs::path& tmp) {
    using hx::Access;
    std::vector<hx::Rule> rules = system_rules();
    rules.push_back({project.string(), Access::Write, true});
    rules.push_back({tmp.string(), Access::Write, true});
    const std::vector<hx::Rule> user = user_rules(options);
    rules.insert(rules.end(), user.begin(), user.end());
    return rules;
}

// True when the project is the home directory or contains it: the model could then write anywhere in home.
bool holds_home(const fs::path& project) {
    std::error_code ec;
    const fs::path home = fs::canonical(env_or("HOME", ""), ec);
    if (ec || home.empty()) {
        return false;
    }
    const fs::path inside = home.lexically_relative(project);
    return !inside.empty() && *inside.begin() != "..";
}

hx::Sandbox confine(const Options& options, const fs::path& project, const fs::path& tmp) {
    if (project == "/" || holds_home(project)) {
        throw UsageError(
            fmt::format("refusing to treat {} as a project: the sandbox would leave your home directory open. "
                        "Run hx inside a project directory.",
                        project.string()));
    }
    try {
        return hx::Sandbox::confine(sandbox_rules(options, project, tmp), options.unix_sockets);
    } catch (const hx::SandboxError& error) {
        throw UsageError(fmt::format("no sandbox: {}. Pass --no-sandbox to run without one.", error.what()));
    }
}

constexpr std::size_t kMaxVmMemory = std::size_t{1} << 30;

// The VM's allocator, with a ceiling: a script that runs away gets "not enough memory" as a Luau error instead
// of taking the machine down. `user` points at the number of bytes in use.
// NOLINTBEGIN(cppcoreguidelines-no-malloc): this is the allocator
void* limited_alloc(void* user, void* block, std::size_t old_size, std::size_t new_size) {
    auto& used = *static_cast<std::size_t*>(user);
    if (new_size == 0) {
        std::free(block);
        used -= old_size;
        return nullptr;
    }
    if (new_size > old_size && used + (new_size - old_size) > kMaxVmMemory) {
        return nullptr;
    }
    void* grown = std::realloc(block, new_size);
    if (grown != nullptr) {
        used = used - old_size + new_size;
    }
    return grown;
}
// NOLINTEND(cppcoreguidelines-no-malloc)

struct StateCloser {
    void operator()(lua_State* L) const noexcept { lua_close(L); }
};
using LuaState = std::unique_ptr<lua_State, StateCloser>;

using Bytecode = std::unique_ptr<char, hx::FreeDeleter>;

// Compiles a chunk and pushes it as a function. On false, the error message is on the stack instead.
bool load_chunk(lua_State* L, std::string_view source, const std::string& name) {
    std::size_t size = 0;
    const Bytecode bytecode(luau_compile(source.data(), source.size(), nullptr, &size));
    if (!bytecode) {
        lua_pushstring(L, "out of memory");
        return false;
    }
    return luau_load(L, name.c_str(), bytecode.get(), size, 0) == 0;
}

std::string pop_message(lua_State* L) {
    std::size_t length = 0;
    const char* text = lua_tolstring(L, -1, &length);
    std::string message = text != nullptr ? std::string(text, length) : std::string("(error is not a string)");
    lua_pop(L, 1);
    return message;
}

// Whether the error on top of the stack is a stop signal, which ends the run whatever was running.
bool is_stop(lua_State* L) {
    const char* text = lua_tostring(L, -1);
    const std::string_view message = text != nullptr ? text : "";
    return message == "interrupted" || message == "terminated";
}

// Turns the error on top of the stack into an exit status, and reports it unless it is a stop signal.
int failure(lua_State* L) {
    const std::string message = pop_message(L);
    if (message == "interrupted") {
        return kExitInterrupted;
    }
    if (message == "terminated") {
        return kExitTerminated;
    }
    fmt::print(stderr, "hx: {}\n", message);
    return kExitFailure;
}

// Whether the value on top of the stack is a module: a table with a `main` function.
bool is_module(lua_State* L) {
    if (!lua_istable(L, -1)) {
        return false;
    }
    lua_getfield(L, -1, "main");
    const bool module = lua_isfunction(L, -1);
    lua_pop(L, 1);
    return module;
}

// Calls main() of the module on top of the stack. Its numeric result is the exit status.
int run_main(lua_State* L) {
    lua_getfield(L, -1, "main");
    if (lua_pcall(L, 0, 1, 0) != 0) {
        return failure(L);
    }
    return lua_isnumber(L, -1) != 0 ? lua_tointeger(L, -1) : 0;
}

// Runs a loop or script. A chunk that returns a table with a `main` function is a module: main() is called and
// its numeric result is the exit status. Anything else is a plain script that has already done its work.
int run_module(lua_State* L, std::string_view source, const std::string& name) {
    if (!load_chunk(L, source, name) || lua_pcall(L, 0, 1, 0) != 0) {
        return failure(L);
    }
    return is_module(L) ? run_main(L) : 0;
}

// Runs the project's loop when it loads and returns a module, the built-in one otherwise, so a loop the model
// broke cannot keep the next session from starting. An error inside main() is a real error, not a reason to
// fall back, and neither is a stop signal while the project's loop loads.
int run_loop(lua_State* L) {
    std::error_code ec;
    if (const std::string project = read_file(kProjectLoop, ec); ec != std::errc::no_such_file_or_directory) {
        std::string why;
        if (ec) {
            why = fmt::format("cannot be read ({})", ec.message());
        } else if (!load_chunk(L, project, fmt::format("={}", kProjectLoop))) {
            why = fmt::format("does not compile ({})", pop_message(L));
        } else if (lua_pcall(L, 0, 1, 0) != 0) {
            if (is_stop(L)) {
                return failure(L);
            }
            why = fmt::format("raised an error while loading ({})", pop_message(L));
        } else if (!is_module(L)) {
            lua_pop(L, 1);
            why = "does not return a table with a main function";
        } else {
            return run_main(L);
        }
        fmt::print(stderr, "hx: {} {}; using the built-in loop\n", kProjectLoop, why);
    }
    return run_module(L, hx::kDefaultLoop, "=loop");
}

// Runs body in a fresh VM that has the host primitives. The Host outlives the VM, and kills what the VM's commands
// left running when it goes.
template <typename Body>
int run_vm(hx::HostConfig config, const hx::Sandbox& sandbox, const Body& body) {
    hx::install_signal_handlers();
    hx::Host host(std::move(config), sandbox);
    std::size_t vm_memory = 0;  // declared before the state, which frees through it
    const LuaState state(lua_newstate(limited_alloc, &vm_memory));
    if (!state) {
        throw std::runtime_error("cannot create a Luau state");
    }
    lua_State* L = state.get();
    luaL_openlibs(L);
    hx::open_json(L);
    host.open(L);
    return body(L);
}

// Forks the judge for --judge KIT. It runs lua/judge.luau in a VM of its own, behind its own wall.
hx::Judge spawn_judge(const Options& options, const std::string& kit_dir, const fs::path& project,
                      const fs::path& tmp) {
    const hx::JudgeWalls walls{
        .system = system_rules(),
        .user = user_rules(options),
        .agent = sandbox_rules(options, project, tmp),
        .unix_sockets = options.unix_sockets,
    };
    return hx::spawn_judge(kit_dir, project, walls, [&](std::vector<std::string> args, const hx::Sandbox& wall) {
        hx::HostConfig config;
        config.args = std::move(args);
        config.quiet = options.quiet;
        config.system_rules = system_rules();
        return run_vm(std::move(config), wall, [](lua_State* L) { return run_module(L, hx::kJudge, "=judge"); });
    });
}

// A descriptor inherited from whoever started hx was opened outside the wall and would pass through it to every
// command. Marking them close-on-exec keeps them from commands; hx itself never uses them.
void close_inherited_fds() {
#ifdef CLOSE_RANGE_CLOEXEC  // glibc 2.34 declares the wrapper and the flag together
    const int done = close_range(3, ~0U, CLOSE_RANGE_CLOEXEC);
#else  // the system call is Linux 5.11, the same number on every architecture
    const long done = syscall(436, 3U, ~0U, 4U);
#endif
    if (done != 0) {
        throw std::runtime_error(fmt::format("cannot mark inherited descriptors close-on-exec: {}", hx::error_text()));
    }
}

int run(const std::vector<std::string_view>& argv) {
    close_inherited_fds();
    const Options options = parse_args(argv);
    if (options.mode == Mode::Help) {
        fmt::print("{}", kUsage);
        return 0;
    }
    if (options.mode == Mode::Version) {
        fmt::print("hx {}\n", hx::kVersion);
        return 0;
    }
    if (options.mode == Mode::Init) {
        return init_project(options.full);
    }

    const ModelChoice endpoint = choose_endpoint(options.model);
    setenv("HX_MODEL", endpoint.model.c_str(), 1);  // NOLINT(concurrency-mt-unsafe): no other thread exists yet
    const bool stdin_tty = isatty(STDIN_FILENO) != 0;
    if (options.no_input && options.mode == Mode::Agent && options.args.empty() && stdin_tty) {
        throw UsageError("no task, and --no-input forbids the prompt: give the task as arguments or on stdin");
    }

    hx::HostConfig config;
    std::string tools_url = env_or("HX_TOOLS_URL", kDeepSeekToolsUrl);
    while (tools_url.ends_with('/')) {
        tools_url.pop_back();
    }
    const Secrets secrets = take_secrets(endpoint.key_env, options.keep_env);
    // host.llm posts to "llm"; host.tool to the others by name. Each key goes to its own URLs only.
    config.endpoints.push_back({.name = "llm",
                                .url = endpoint.url,
                                .auth = hx::Auth::ApiKey,
                                .key = secrets.key,
                                .key_env = "",  // a local proxy may need none
                                .headers = {"anthropic-version: 2023-06-01"},
                                .form = false});
    for (const char* name : {"search", "fetch", "ocr"}) {
        config.endpoints.push_back({.name = name,
                                    .url = fmt::format("{}/{}", tools_url, name),
                                    .auth = hx::Auth::Bearer,
                                    .key = secrets.tools_key,
                                    .key_env = kDeepSeekKeyEnv,
                                    .headers = {},
                                    .form = true});
    }
    config.endpoints.push_back({.name = "exa",
                                .url = env_or("HX_EXA_URL", kExaUrl),
                                .auth = hx::Auth::ApiKey,
                                .key = secrets.exa_key,
                                .key_env = kExaKeyEnv,
                                .headers = {},
                                .form = false});
    config.model = endpoint.model;
    config.args = options.args;
    config.interactive = options.mode == Mode::Agent && options.args.empty() && stdin_tty && !options.no_input;
    config.quiet = options.quiet;
    config.default_loop = hx::kDefaultLoop;
    for (const auto& [name, source] : hx::kExtensions) {
        config.extensions.emplace_back(name, source);
    }
    // libcurl does not read these variables itself; the wall lets the file be read (see sandbox_rules).
    config.ca_bundle = env_or("SSL_CERT_FILE", env_or("CURL_CA_BUNDLE", ""));
    if (access(config.ca_bundle.c_str(), R_OK) != 0) {
        config.ca_bundle.clear();  // a stale variable must not break every request; libcurl's own roots then apply
    }

    std::optional<std::string> script;
    if (options.mode == Mode::Script) {
        if (options.script == "-") {
            if (isatty(STDIN_FILENO) != 0) {
                throw UsageError("--run - reads the script from stdin, which is a terminal; pipe the script in");
            }
            script = std::string(std::istreambuf_iterator<char>(std::cin), std::istreambuf_iterator<char>());
        } else {
            std::error_code ec;
            script = read_file(options.script, ec);
            if (ec) {
                throw UsageError(fmt::format("cannot read {}: {}", options.script, ec.message()));
            }
        }
    }

    std::error_code ec;
    const fs::path project = fs::canonical(fs::current_path(ec), ec);
    if (ec) {
        throw UsageError(fmt::format("cannot resolve the current directory: {}", ec.message()));
    }
    const TempDir tmp;

    // The model's words go to stdout and progress to stderr; line buffering keeps the two in order in a pipe.
    static_cast<void>(std::setvbuf(stdout, nullptr, _IOLBF, 0));
    const std::optional<hx::Judge> judge =
        options.judge ? std::optional(spawn_judge(options, *options.judge, project, tmp.path())) : std::nullopt;
    const CurlGlobal curl;
    const hx::Sandbox sandbox = options.sandbox ? confine(options, project, tmp.path()) : hx::Sandbox::none();
    if (sandbox.confined() && !sandbox.blocks_unix_sockets()) {
        fmt::print(stderr,
                   "hx: note: this kernel's Landlock (ABI {}) cannot stop commands from connecting to unix sockets "
                   "such as the D-Bus session bus or ssh-agent; ABI {} can. See \"The wall\" in the README.\n",
                   sandbox.abi(), hx::Sandbox::kUnixSocketAbi);
    }
    // Keeps the commands the model runs from reading the key out of this process's memory.
    prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
    config.system_rules = system_rules();
    if (judge) {
        config.judge = hx::UniqueFd(judge->channel);
    }
    return run_vm(std::move(config), sandbox, [&](lua_State* L) {
        const int status = script ? run_module(L, *script, fmt::format("={}", options.script)) : run_loop(L);
        if (judge) {
            hx::finish_judge(*judge);
        }
        return status;
    });
}

// Reports a fatal error without being able to fail itself.
int report(const char* message, int status) noexcept {
    static_cast<void>(std::fputs("hx: ", stderr));
    static_cast<void>(std::fputs(message, stderr));
    static_cast<void>(std::fputc('\n', stderr));
    return status;
}

}  // namespace

int main(int argc, char** argv) {
    try {
        // NOLINTNEXTLINE(cppcoreguidelines-pro-bounds-pointer-arithmetic): argv is the C entry-point array
        return run(std::vector<std::string_view>(argv + std::min(argc, 1), argv + argc));
    } catch (const UsageError& error) {
        return report(error.what(), kExitUsage);
    } catch (const std::exception& error) {
        return report(error.what(), kExitFailure);
    } catch (...) {
        return report("unknown error", kExitFailure);
    }
}
