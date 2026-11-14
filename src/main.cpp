// hx: a small host for a per-project coding agent.
//
// The host is a Luau VM plus a handful of primitives (model call, shell, files). The agent loop itself is a
// Luau file the project owns. Before any of that runs, the process confines itself with Landlock, so the VM,
// the file primitives and every command the model starts share one set of walls.

#include <curl/curl.h>
#include <sys/prctl.h>
#include <unistd.h>

#include <algorithm>
#include <cctype>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <exception>
#include <filesystem>
#include <fstream>
#include <initializer_list>
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
#include "lua.h"
#include "luacode.h"
#include "lualib.h"
#include "sandbox.h"
#include "sys.h"

namespace {

namespace fs = std::filesystem;

constexpr std::string_view kUsage = R"(usage:
  hx [options] [TASK...]             run the agent on TASK; with no task, read it from stdin or start a prompt
  hx [options] run FILE [ARGS...]    run a Luau script with the host primitives
  hx init [--full]                   write .hx/loop.luau, a loop that starts from the built-in one; with
                                     --full, a copy of the whole built-in loop

options:
  --ro PATH        also allow reading PATH (a toolchain in your home directory, for example)
  --rw PATH        also allow writing PATH
  --keep-env NAME  keep a variable that looks like a secret (its name ends in KEY, TOKEN, SECRET, PASSWORD
                   or CREDENTIALS) in the environment of commands
  --no-sandbox     run without Landlock (the model can then touch anything you can)
  -h, --help       show this text

environment:
  HX_API_URL       Anthropic Messages endpoint (default https://api.deepseek.com/anthropic/v1/messages)
  HX_KEY_ENV       name of the variable that holds the API key (default DEEPSEEK_API_KEY)
  HX_MODEL         model name (default deepseek-flash)
)";

constexpr const char* kDefaultUrl = "https://api.deepseek.com/anthropic/v1/messages";
constexpr const char* kDefaultKeyEnv = "DEEPSEEK_API_KEY";
constexpr const char* kProjectLoop = ".hx/loop.luau";

constexpr int kExitFailure = 1;
constexpr int kExitUsage = 2;
constexpr int kExitInterrupted = 130;  // 128 + SIGINT, as a shell reports it
constexpr int kExitTerminated = 143;   // 128 + SIGTERM

// A problem the user has to fix before hx can start: reported on stderr, exit status 2.
class UsageError : public std::runtime_error {
  public:
    using std::runtime_error::runtime_error;
};

enum class Mode : std::uint8_t { Agent, Script, Init, Help };

struct Options {
    Mode mode = Mode::Agent;
    std::string script;
    std::vector<std::string> args;
    std::vector<std::string> read_paths;
    std::vector<std::string> write_paths;
    std::vector<std::string> keep_env;
    bool sandbox = true;
    bool full = false;  // init: copy the whole built-in loop, not the overlay
};

Options parse_args(const std::vector<std::string_view>& argv) {
    Options options;
    std::size_t i = 0;
    const auto operand = [&](std::string_view flag) {
        if (i + 1 >= argv.size()) {
            throw UsageError(fmt::format("{} needs a value", flag));
        }
        return std::string(argv[++i]);
    };
    for (; i < argv.size(); ++i) {
        const std::string_view arg = argv[i];
        if (arg == "--ro") {
            options.read_paths.push_back(operand(arg));
        } else if (arg == "--rw") {
            options.write_paths.push_back(operand(arg));
        } else if (arg == "--keep-env") {
            options.keep_env.push_back(operand(arg));
        } else if (arg == "--no-sandbox") {
            options.sandbox = false;
        } else if (arg == "-h" || arg == "--help") {
            options.mode = Mode::Help;
            return options;
        } else if (arg == "--") {
            ++i;
            break;
        } else if (arg.size() > 1 && arg.starts_with('-')) {
            throw UsageError(fmt::format("unknown option {} (see --help)", arg));
        } else {
            break;
        }
    }
    if (i < argv.size() && argv[i] == "init" &&
        (i + 1 == argv.size() || (i + 2 == argv.size() && argv[i + 1] == "--full"))) {
        options.mode = Mode::Init;
        options.full = i + 2 == argv.size();
        return options;
    }
    if (i < argv.size() && argv[i] == "run") {
        if (i + 1 >= argv.size()) {
            throw UsageError("run needs a file");
        }
        options.mode = Mode::Script;
        options.script = argv[i + 1];
        i += 2;
    }
    options.args.assign(argv.begin() + static_cast<std::ptrdiff_t>(i), argv.end());
    return options;
}

std::optional<std::string> read_file(const fs::path& path) {
    std::ifstream in(path, std::ios::binary);
    if (!in) {
        return std::nullopt;
    }
    return std::string(std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>());
}

// Writes .hx/loop.luau: by default the overlay, which loads the built-in loop and changes parts of it, so fixes
// to hx still reach the project; with `full`, a copy of the whole built-in loop, which the project then owns.
int init_project(bool full) {
    std::error_code ec;
    fs::create_directories(".hx/ext", ec);
    if (ec) {
        throw UsageError(fmt::format(".hx/ext: {}", ec.message()));
    }
    if (fs::exists(kProjectLoop)) {
        throw UsageError(fmt::format("{} already exists; it is yours, so it stays", kProjectLoop));
    }
    std::ofstream(kProjectLoop, std::ios::binary) << (full ? hx::kDefaultLoop : hx::kLoopOverlay);
    std::ofstream(".hx/.gitignore") << "log/\n";
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

// Returns the API key and removes it, and every other variable that looks like a secret, from the environment.
// Commands the model runs inherit the environment, and with the network open an inherited secret is a leaked
// one. The entries are also wiped where they sit, because /proc/PID/environ shows the original memory.
std::string take_secrets(std::string_view key_env, const std::vector<std::string>& keep) {
    std::string key;
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
            key = text.substr(equals + 1);
        }
        const bool kept = std::find(keep.begin(), keep.end(), name) != keep.end();
        if (is_key || (looks_secret(name) && !kept)) {
            doomed.emplace_back(name, *entry);
        }
    }
    for (const auto& [name, entry] : doomed) {
        unsetenv(name.c_str());  // NOLINT(concurrency-mt-unsafe): no other thread exists yet
        std::memset(entry, 0, std::strlen(entry));
    }
    return key;
}

std::string env_or(const char* name, std::string_view fallback) {
    const char* value = std::getenv(name);  // NOLINT(concurrency-mt-unsafe): no other thread exists yet
    return std::string(value != nullptr && *value != '\0' ? value : fallback);
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
    TempDir(TempDir&&) = delete;
    TempDir& operator=(TempDir&&) = delete;
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
    CurlGlobal(CurlGlobal&&) = delete;
    CurlGlobal& operator=(CurlGlobal&&) = delete;
    ~CurlGlobal() { curl_global_cleanup(); }
};

// The walls: system directories are readable, the project and a private temporary directory are writable, and
// the rest of the machine (the home directory above all) cannot be opened at all.
std::vector<hx::Rule> sandbox_rules(const Options& options, const fs::path& project, const fs::path& tmp) {
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
    rules.push_back({project.string(), Access::Write, true});
    rules.push_back({tmp.string(), Access::Write, true});
    for (const std::string& path : options.read_paths) {
        rules.push_back({path, Access::Read, true});
    }
    for (const std::string& path : options.write_paths) {
        rules.push_back({path, Access::Write, true});
    }
    return rules;
}

hx::Sandbox confine(const Options& options, const fs::path& project, const fs::path& tmp) {
    std::error_code ec;
    const std::string home = env_or("HOME", "");
    if (project == "/" || (!home.empty() && fs::equivalent(project, home, ec))) {
        throw UsageError(
            fmt::format("refusing to treat {} as a project: the sandbox would wall nothing off. "
                        "Run hx inside a project directory.",
                        project.string()));
    }
    try {
        return hx::Sandbox::confine(sandbox_rules(options, project, tmp));
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
    if (std::optional<std::string> project = read_file(kProjectLoop)) {
        std::string why;
        if (!load_chunk(L, *project, fmt::format("={}", kProjectLoop))) {
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

int run(const std::vector<std::string_view>& argv) {
    const Options options = parse_args(argv);
    if (options.mode == Mode::Help) {
        fmt::print("{}", kUsage);
        return 0;
    }
    if (options.mode == Mode::Init) {
        return init_project(options.full);
    }

    hx::HostConfig config;
    config.api_url = env_or("HX_API_URL", kDefaultUrl);
    config.api_key = take_secrets(env_or("HX_KEY_ENV", kDefaultKeyEnv), options.keep_env);
    config.args = options.args;
    config.interactive = options.mode == Mode::Agent && options.args.empty() && isatty(STDIN_FILENO) != 0;
    config.default_loop = hx::kDefaultLoop;
    // A proxy's trust root usually lives in the home directory, which is about to become unreadable.
    for (const char* name : {"SSL_CERT_FILE", "CURL_CA_BUNDLE"}) {
        if (const std::string path = env_or(name, ""); !path.empty() && config.ca_bundle.empty()) {
            config.ca_bundle = read_file(path).value_or("");
        }
    }

    std::optional<std::string> script;
    if (options.mode == Mode::Script) {
        script = read_file(options.script);
        if (!script) {
            throw UsageError(fmt::format("cannot read {}", options.script));
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
    hx::install_signal_handlers();

    hx::Host host(std::move(config), sandbox);  // outlives the VM; kills what the model left running when it goes
    std::size_t vm_memory = 0;                  // declared before the state, which frees through it
    const LuaState state(lua_newstate(limited_alloc, &vm_memory));
    if (!state) {
        throw std::runtime_error("cannot create a Luau state");
    }
    lua_State* L = state.get();
    luaL_openlibs(L);
    hx::open_json(L);
    host.open(L);

    if (script) {
        return run_module(L, *script, fmt::format("={}", options.script));
    }
    return run_loop(L);
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
