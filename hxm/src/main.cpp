// hxm: a small host for a per-project coding agent.
//
// The host is a Luau VM plus a handful of primitives (model call, shell, files). The agent loop itself is a
// Luau file the project owns. Before any of that runs, the process confines itself with Landlock, so the VM,
// the file primitives and every command the model starts share one set of walls.

#include <algorithm>
#include <cctype>
#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <system_error>
#include <vector>

#include <curl/curl.h>
#include <sys/prctl.h>
#include <unistd.h>

#include "embedded.h"
#include "host.h"
#include "json.h"
#include "sandbox.h"

#include "lua.h"
#include "luacode.h"
#include "lualib.h"

extern char** environ;

namespace {

namespace fs = std::filesystem;

constexpr char kUsage[] = R"(usage:
  hxm [options] [TASK...]     run the agent on TASK; with no task, read it from stdin or start a prompt
  hxm [options] run FILE [ARGS...]   run a Luau script with the host primitives
  hxm init                    copy the built-in loop to .hxm/loop.luau so this project owns its harness

options:
  --ro PATH       also allow reading PATH (a toolchain in your home directory, for example)
  --rw PATH       also allow writing PATH
  --keep-env NAME keep a variable that looks like a secret (its name ends in KEY, TOKEN, SECRET, PASSWORD
                  or CREDENTIALS) in the environment of commands
  --no-sandbox    run without Landlock (the model can then touch anything you can)

environment:
  HXM_API_URL     chat-completions endpoint (default https://api.deepseek.com/chat/completions)
  HXM_KEY_ENV     name of the variable that holds the API key (default DEEPSEEK_API_KEY)
  HXM_MODEL       model name (default deepseek-flash)
)";

constexpr char kDefaultUrl[] = "https://api.deepseek.com/chat/completions";
constexpr char kDefaultKeyEnv[] = "DEEPSEEK_API_KEY";
constexpr char kProjectLoop[] = ".hxm/loop.luau";

enum class Mode { Agent, Script, Init };

struct Options {
    Mode mode = Mode::Agent;
    std::string script;
    std::vector<std::string> args;
    std::vector<std::string> read_paths;
    std::vector<std::string> write_paths;
    std::vector<std::string> keep_env;
    bool sandbox = true;
};

[[noreturn]] void die(const std::string& message) {
    std::fprintf(stderr, "hxm: %s\n", message.c_str());
    std::exit(2);
}

Options parse_args(int argc, char** argv) {
    Options options;
    int i = 1;
    const auto operand = [&](const char* flag) -> std::string {
        if (i + 1 >= argc) die(std::string(flag) + " needs a value");
        return argv[++i];
    };
    for (; i < argc; ++i) {
        const std::string_view arg = argv[i];
        if (arg == "--ro") options.read_paths.push_back(operand("--ro"));
        else if (arg == "--rw") options.write_paths.push_back(operand("--rw"));
        else if (arg == "--keep-env") options.keep_env.push_back(operand("--keep-env"));
        else if (arg == "--no-sandbox") options.sandbox = false;
        else if (arg == "-h" || arg == "--help") {
            std::fputs(kUsage, stdout);
            std::exit(0);
        } else if (arg == "--") {
            ++i;
            break;
        } else if (arg.starts_with("-") && arg.size() > 1) {
            die("unknown option " + std::string(arg) + " (see --help)");
        } else {
            break;
        }
    }
    if (i < argc && std::string_view(argv[i]) == "init" && i + 1 == argc) {
        options.mode = Mode::Init;
        return options;
    }
    if (i < argc && std::string_view(argv[i]) == "run") {
        if (i + 1 >= argc) die("run needs a file");
        options.mode = Mode::Script;
        options.script = argv[i + 1];
        i += 2;
    }
    options.args.assign(argv + i, argv + argc);
    return options;
}

std::optional<std::string> read_file(const fs::path& path) {
    std::ifstream in(path, std::ios::binary);
    if (!in) return std::nullopt;
    return std::string(std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>());
}

int init_project() {
    std::error_code ec;
    fs::create_directories(".hxm/ext", ec);
    if (ec) die(".hxm/ext: " + ec.message());
    if (fs::exists(kProjectLoop)) die(std::string(kProjectLoop) + " already exists; it is yours, so it stays");
    std::ofstream(kProjectLoop, std::ios::binary) << hxm::kDefaultLoop;
    std::ofstream(".hxm/.gitignore") << "log/\n";
    std::printf("wrote %s: that file is this project's harness now, edit it freely\n", kProjectLoop);
    return 0;
}

// A variable whose name ends in one of these words is treated as a secret. Matching the end, not any part, keeps
// plumbing such as GIT_CONFIG_KEY_0 or AWS_ACCESS_KEY_ID in place.
bool looks_secret(std::string name) {
    std::transform(name.begin(), name.end(), name.begin(), [](unsigned char c) { return std::toupper(c); });
    for (const std::string_view mark : {"KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIALS"}) {
        if (!name.ends_with(mark)) continue;
        if (name.size() == mark.size() || name[name.size() - mark.size() - 1] == '_') return true;
    }
    return false;
}

// Returns the API key and removes it, and every other variable that looks like a secret, from the environment.
// Commands the model runs inherit the environment, and with the network open an inherited secret is a leaked
// one. The entries are also wiped where they sit, because /proc/PID/environ shows the original memory.
std::string take_secrets(const std::string& key_env, const std::vector<std::string>& keep) {
    std::string key;
    std::vector<std::pair<std::string, char*>> doomed;
    for (char** entry = environ; *entry != nullptr; ++entry) {
        const char* const equals = std::strchr(*entry, '=');
        if (equals == nullptr) continue;
        const std::string name(*entry, static_cast<size_t>(equals - *entry));
        const bool is_key = name == key_env;
        if (is_key) key = equals + 1;
        const bool kept = std::find(keep.begin(), keep.end(), name) != keep.end();
        if (is_key || (looks_secret(name) && !kept)) doomed.emplace_back(name, *entry);
    }
    for (const auto& [name, entry] : doomed) {
        unsetenv(name.c_str());
        std::memset(entry, 0, std::strlen(entry));
    }
    return key;
}

std::string env_or(const char* name, const char* fallback) {
    const char* value = std::getenv(name);
    return value != nullptr && *value != '\0' ? value : fallback;
}

// The walls: system directories are readable, the project and a private temporary directory are writable, and
// the rest of the machine (the home directory above all) cannot be opened at all.
std::vector<hxm::Rule> sandbox_rules(const Options& options, const fs::path& project, const fs::path& tmp) {
    using hxm::Access;
    std::vector<hxm::Rule> rules;
    for (const char* path : {"/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/opt", "/proc",
                             "/sys", "/dev", "/nix/store", "/run/systemd/resolve"})
        rules.push_back({path, Access::Read, false});
    for (const char* path : {"/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty",
                             "/dev/ptmx", "/dev/pts"})
        rules.push_back({path, Access::Device, false});
    // git refuses to run when it cannot read its configuration, and git is how the model's work is undone.
    if (const char* home = std::getenv("HOME"); home != nullptr && *home == '/') {
        rules.push_back({std::string(home) + "/.gitconfig", Access::Read, false});
        rules.push_back({std::string(home) + "/.config/git", Access::Read, false});
    }
    rules.push_back({project.string(), Access::Write, true});
    rules.push_back({tmp.string(), Access::Write, true});
    for (const std::string& path : options.read_paths) rules.push_back({path, Access::Read, true});
    for (const std::string& path : options.write_paths) rules.push_back({path, Access::Write, true});
    return rules;
}

void confine(const Options& options, const fs::path& project, const fs::path& tmp) {
    std::error_code ec;
    const char* home = std::getenv("HOME");
    if (project == "/" || (home != nullptr && fs::equivalent(project, home, ec)))
        die("refusing to treat " + project.string() + " as a project: the sandbox would wall nothing off. "
            "Run hxm inside a project directory.");
    const hxm::SandboxResult result = hxm::apply_sandbox(sandbox_rules(options, project, tmp));
    if (result.abi == 0) die("no sandbox: " + result.error + ". Pass --no-sandbox to run without one.");
}

struct StateCloser {
    void operator()(lua_State* L) const { lua_close(L); }
};
struct FreeDeleter {
    void operator()(char* p) const { std::free(p); }
};

// Compiles and runs a chunk, leaving its first return value on the stack. On false the error message is there.
bool run_chunk(lua_State* L, const std::string& source, const std::string& name) {
    size_t size = 0;
    const std::unique_ptr<char, FreeDeleter> bytecode(luau_compile(source.data(), source.size(), nullptr, &size));
    if (!bytecode) {
        lua_pushstring(L, "out of memory");
        return false;
    }
    if (luau_load(L, name.c_str(), bytecode.get(), size, 0) != 0) return false;
    return lua_pcall(L, 0, 1, 0) == 0;
}

std::string pop_message(lua_State* L) {
    size_t length = 0;
    const char* text = lua_tolstring(L, -1, &length);
    std::string message = text != nullptr ? std::string(text, length) : std::string("(error is not a string)");
    lua_pop(L, 1);
    return message;
}

// Runs a loop or script. A chunk that returns a table with a `main` function is a module: main() is called and
// its numeric result is the exit status. Anything else is a plain script that has already done its work.
int run_module(lua_State* L, const std::string& source, const std::string& name) {
    const auto failure = [&]() {
        const std::string message = pop_message(L);
        if (message == "interrupted") return 130;
        if (message == "terminated") return 143;
        std::fprintf(stderr, "hxm: %s\n", message.c_str());
        return 1;
    };
    if (!run_chunk(L, source, name)) return failure();
    if (!lua_istable(L, -1)) return 0;
    lua_getfield(L, -1, "main");
    if (!lua_isfunction(L, -1)) return 0;
    if (lua_pcall(L, 0, 1, 0) != 0) return failure();
    return lua_isnumber(L, -1) ? static_cast<int>(lua_tonumber(L, -1)) : 0;
}

// Picks the loop: the project's own when it at least compiles, the built-in one otherwise, so a loop the model
// broke cannot keep the next session from starting.
std::pair<std::string, std::string> choose_loop(lua_State* L) {
    const std::optional<std::string> project = read_file(kProjectLoop);
    if (!project) return {hxm::kDefaultLoop, "=loop"};
    size_t size = 0;
    const std::unique_ptr<char, FreeDeleter> bytecode(luau_compile(project->data(), project->size(), nullptr, &size));
    if (bytecode && luau_load(L, "=.hxm/loop.luau", bytecode.get(), size, 0) == 0) {
        lua_pop(L, 1);
        return {*project, "=.hxm/loop.luau"};
    }
    std::fprintf(stderr, "hxm: %s does not compile (%s); using the built-in loop\n", kProjectLoop,
                 bytecode ? pop_message(L).c_str() : "out of memory");
    return {hxm::kDefaultLoop, "=loop"};
}

} // namespace

int main(int argc, char** argv) {
    const Options options = parse_args(argc, argv);
    if (options.mode == Mode::Init) return init_project();

    hxm::HostConfig config;
    config.api_url = env_or("HXM_API_URL", kDefaultUrl);
    config.api_key = take_secrets(env_or("HXM_KEY_ENV", kDefaultKeyEnv), options.keep_env);
    config.args = options.args;
    config.interactive = options.mode == Mode::Agent && options.args.empty() && isatty(STDIN_FILENO) != 0;
    config.default_loop = hxm::kDefaultLoop;
    // A proxy's trust root usually lives in the home directory, which is about to become unreadable.
    for (const char* name : {"SSL_CERT_FILE", "CURL_CA_BUNDLE"}) {
        if (const char* path = std::getenv(name); path != nullptr && config.ca_bundle.empty())
            config.ca_bundle = read_file(path).value_or("");
    }

    std::optional<std::string> script;
    if (options.mode == Mode::Script) {
        script = read_file(options.script);
        if (!script) die("cannot read " + options.script);
    }

    std::error_code ec;
    const fs::path project = fs::canonical(fs::current_path(), ec);
    if (ec) die("cannot resolve the current directory: " + ec.message());
    std::string tmp_template = (fs::temp_directory_path(ec) / "hxm-XXXXXX").string();
    if (mkdtemp(tmp_template.data()) == nullptr) die("cannot create a temporary directory: " + tmp_template);
    const fs::path tmp = tmp_template;
    setenv("TMPDIR", tmp.c_str(), 1);

    // The model's words go to stdout and progress to stderr; line buffering keeps the two in order in a pipe.
    std::setvbuf(stdout, nullptr, _IOLBF, 0);
    curl_global_init(CURL_GLOBAL_DEFAULT);
    if (options.sandbox) confine(options, project, tmp);
    // Keeps the commands the model runs from reading the key out of this process's memory.
    prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
    hxm::install_signal_handlers();

    int status = 0;
    {
        const std::unique_ptr<lua_State, StateCloser> state(luaL_newstate());
        lua_State* L = state.get();
        luaL_openlibs(L);
        hxm::open_json(L);
        hxm::open_host(L, config);
        if (script) {
            status = run_module(L, *script, "=" + options.script);
        } else {
            const auto [source, name] = choose_loop(L);
            status = run_module(L, source, name);
        }
    }

    hxm::kill_children();
    fs::remove_all(tmp, ec);
    curl_global_cleanup();
    return status;
}
