#include <curl/curl.h>
#include <fcntl.h>
#include <sys/prctl.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cctype>
#include <cerrno>
#include <charconv>
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
#include <map>
#include <memory>
#include <optional>
#include <ranges>
#include <set>
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
                                      start a prompt when stdin is a terminal. With a task, stdin
                                      is not read: it is left for the model's code
  hx [options] --run FILE [ARGS...]   run a Luau script with the host primitives (FILE - reads stdin);
                                      every word after FILE goes to the script, even one starting with -
  hx --init [--full]                  write .hx/loop.luau, a loop that starts from the built-in one;
                                      with --full, a copy of the whole built-in loop
  hx --judge KIT [options] [TASK...]  run the agent with a judge: only changes it submits and the kit's
                                      measurements confirm count; KIT must lie outside the wall
  hx --rpi [options] [TASK...]        run the agent as a lead, on Opus: it researches and plans, agents
                                      on deepseek-flash implement side by side, and it verifies their work

examples:
  hx "add a --json flag to the export command and test it"
  hx --rpi "move the config loader to TOML, with its tests and docs"
  { echo review this change; git diff; } | hx
  hx --ro ~/.rustup --ro ~/.cargo --rw ~/.cargo/registry "fix the failing test"
  hx -q "summarise the README" > summary.txt

options:
  --model NAME      model to ask (overrides HX_MODEL): opus, sonnet, haiku or a full name
  --judge KIT       judge the agent's changes with the kit in directory KIT (run, and optionally build
                    and check); the agent cannot read the kit, and the champion, its patch and the
                    ledger go to a directory outside the wall
  --rpi             lead the task by the rpi workflow (research, plan, implement): the model is opus
                    unless --model names another, whatever HX_MODEL says, and its agents run on
                    deepseek-flash when DEEPSEEK_API_KEY is set, on the lead's model otherwise
  --resume LOG      go on from a saved conversation: LOG is a file in .hx/log, or `last` for the newest
  --ro PATH        also allow reading PATH (a toolchain in your home directory, for example)
  --rw PATH         also allow writing PATH
  --keep-env NAME   keep a variable that looks like a secret (its name ends in KEY, TOKEN, SECRET,
                    PASSWORD, PASSWD, CREDENTIALS, _PAT or _PWD) in the environment of commands
  --unix-sockets    before Landlock ABI 9, let commands create unix sockets; they can then connect to
                    any socket by path, the D-Bus session bus and ssh-agent included
  --no-sandbox      run without Landlock (the model can then touch anything you can)
  --built-in-loop   run the loop built into hx and leave .hx/loop.luau unread; without this, a
                    .hx/loop.luau that does not load stops hx
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
  HX_DEEPSEEK_URL   DeepSeek's Messages endpoint, which a DeepSeek model under a session on a Claude
                    model is asked through, an agent say, with DEEPSEEK_API_KEY (default
                    https://api.deepseek.com/anthropic/v1/messages)
  HX_ANTHROPIC_URL  Anthropic's Messages endpoint, which a Claude model under a session on another
                    model is asked through, with ANTHROPIC_API_KEY (default
                    https://api.anthropic.com/v1/messages)

output:
  The model's words go to stdout; progress, warnings and errors go to stderr.

exit status:
  0 done, 1 the run failed, 2 bad usage or hx cannot start here, 130 interrupted, 143 terminated

docs: https://github.com/voladelta/luau-harness
)";

struct Vendor {
    std::string_view name;
    const char* url;
    const char* url_env;
    const char* key_env;
    const char* version;
    const char* model;
};
constexpr Vendor kVendors[] = {
    {
        .name = "anthropic",
        .url = "https://api.anthropic.com/v1/messages",
        .url_env = "HX_ANTHROPIC_URL",
        .key_env = "ANTHROPIC_API_KEY",
        .version = "anthropic-version: 2026-10-21",
        .model = "claude-opus-5-5",
    },
    {
        .name = "deepseek",
        .url = "https://api.deepseek.com/anthropic/v1/messages",
        .url_env = "HX_DEEPSEEK_URL",
        .key_env = "DEEPSEEK_API_KEY",
        .version = "anthropic-version: 2023-06-01",
        .model = "deepseek-flash",
    },
};
constexpr const Vendor& kAnthropic = kVendors[0];
constexpr const Vendor& kDeepSeek = kVendors[1];

struct Service {
    const char* name;
    const char* url;
    const char* url_env;
    const char* path;
    const char* key_env;
    hx::Auth auth;
};
constexpr Service kServices[] = {
    {
        .name = "search",
        .url = "https://api.deepseek.com/v1/tools",
        .url_env = "HX_TOOLS_URL",
        .path = "/search",
        .key_env = kDeepSeek.key_env,
        .auth = hx::Auth::Bearer,
    },
    {
        .name = "fetch",
        .url = "https://api.deepseek.com/v1/tools",
        .url_env = "HX_TOOLS_URL",
        .path = "/fetch",
        .key_env = kDeepSeek.key_env,
        .auth = hx::Auth::Bearer,
    },
    {
        .name = "ocr",
        .url = "https://api.deepseek.com/v1/tools",
        .url_env = "HX_TOOLS_URL",
        .path = "/ocr",
        .key_env = kDeepSeek.key_env,
        .auth = hx::Auth::Bearer,
    },
    {
        .name = "exa",
        .url = "https://api.exa.ai/search",
        .url_env = "HX_EXA_URL",
        .path = "",
        .key_env = "EXA_API_KEY",
        .auth = hx::Auth::ApiKey,
    },
};

const Vendor& vendor_of(std::string_view model) {
    const auto* const found = std::ranges::find(kVendors, hx::vendor_of(model), &Vendor::name);
    return found != std::end(kVendors) ? *found : kDeepSeek;
}

constexpr std::pair<std::string_view, std::string_view> kModelAliases[] = {
    {"opus", "claude-opus-5-5"},
    {"sonnet", "claude-sonnet-5-5"},
    {"haiku", "claude-haiku-5-5"},
};
constexpr std::string_view kRpiLead = "opus";
constexpr const char* kRpiWorkflow = "rpi";
constexpr std::string_view kEfforts[] = {"low", "medium", "high", "xhigh", "max"};
constexpr const char* kProjectLoop = ".hx/loop.luau";

constexpr int kExitInterrupted = 130;
constexpr int kExitTerminated = 143;

using hx::env_or;
using hx::kExitFailure;
using hx::kExitUsage;
using hx::UsageError;

enum class Mode : std::uint8_t { Agent, Script, Init, Help, Version };

struct Options {
    Mode mode = Mode::Agent;
    std::string script;
    std::vector<std::string> args;
    std::vector<std::string> read_paths;
    std::vector<std::string> write_paths;
    std::vector<std::string> keep_env;
    std::optional<std::string> model;
    std::optional<std::string> judge;
    std::optional<std::string> resume;
    bool rpi = false;
    bool built_in_loop = false;
    bool sandbox = true;
    bool unix_sockets = false;
    bool full = false;
    bool quiet = false;
    bool no_input = false;
};

constexpr std::string_view kOptionNames[] = {
    "--model",   "--ro",       "--rw",   "--keep-env",      "--unix-sockets", "--no-sandbox",
    "--quiet",   "--no-input", "--init", "--full",          "--run",          "--help",
    "--version", "--judge",    "--rpi",  "--built-in-loop", "--resume",
};

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
    std::size_t best_distance = 3;
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

class OptionWord {
  public:
    OptionWord(const std::vector<std::string_view>& argv, std::size_t& at) : argv_(argv), at_(at), name_(argv[at]) {
        if (const std::size_t equals = name_.find('='); name_.starts_with("--") && equals != std::string_view::npos) {
            inline_value_ = name_.substr(equals + 1);
            name_ = name_.substr(0, equals);
        }
    }
    OptionWord(const OptionWord&) = delete;
    OptionWord& operator=(const OptionWord&) = delete;
    ~OptionWord() = default;

    [[nodiscard]] std::string_view name() const noexcept { return name_; }

    std::string value(std::string_view metavar) {
        if (inline_value_ ? inline_value_->empty() : at_ + 1 >= argv_.size()) {
            throw UsageError(fmt::format("{} needs a value: {} {}", name_, name_, metavar));
        }
        return std::string(inline_value_ ? *inline_value_ : argv_[++at_]);
    }

    void flag() const {
        if (inline_value_) {
            throw UsageError(fmt::format("{} takes no value", name_));
        }
    }

  private:
    const std::vector<std::string_view>& argv_;
    std::size_t& at_;
    std::string_view name_;
    std::optional<std::string_view> inline_value_;
};

void check_combinations(const Options& options) {
    if (options.full && options.mode != Mode::Init) {
        throw UsageError("--full goes with --init");
    }
    if (options.mode == Mode::Init && !options.args.empty()) {
        throw UsageError("--init takes no task");
    }
    if (options.rpi && options.mode != Mode::Agent) {
        throw UsageError("--rpi leads the agent on a task: it goes with neither --run nor --init");
    }
    if (options.built_in_loop && options.mode != Mode::Agent) {
        throw UsageError("--built-in-loop picks the agent's loop: it goes with neither --run nor --init");
    }
    if (options.judge && !options.sandbox) {
        throw UsageError("--judge needs the wall: without it the agent could read the kit and rewrite the ledger");
    }
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
            break;
        }
        OptionWord option(argv, i);
        const std::string_view arg = option.name();
        if (arg == "--ro") {
            options.read_paths.push_back(option.value("PATH"));
        } else if (arg == "--rw") {
            options.write_paths.push_back(option.value("PATH"));
        } else if (arg == "--keep-env") {
            options.keep_env.push_back(option.value("NAME"));
        } else if (arg == "--model") {
            options.model = option.value("NAME");
        } else if (arg == "--judge") {
            options.judge = option.value("KIT");
        } else if (arg == "--resume") {
            options.resume = option.value("LOG");
        } else if (arg == "--rpi") {
            option.flag();
            options.rpi = true;
        } else if (arg == "--built-in-loop") {
            option.flag();
            options.built_in_loop = true;
        } else if (arg == "--unix-sockets") {
            option.flag();
            options.unix_sockets = true;
        } else if (arg == "--no-sandbox") {
            option.flag();
            options.sandbox = false;
        } else if (arg == "-q" || arg == "--quiet") {
            option.flag();
            options.quiet = true;
        } else if (arg == "--no-input") {
            option.flag();
            options.no_input = true;
        } else if (arg == "-h" || arg == "--help") {
            options.mode = Mode::Help;
            return options;
        } else if (arg == "--version") {
            options.mode = Mode::Version;
            return options;
        } else if (arg == "--init") {
            option.flag();
            if (options.mode == Mode::Script) {
                throw UsageError("--run and --init do not go together");
            }
            options.mode = Mode::Init;
        } else if (arg == "--full") {
            option.flag();
            options.full = true;
        } else if (arg == "--run") {
            if (options.mode == Mode::Init) {
                throw UsageError("--run and --init do not go together");
            }
            options.mode = Mode::Script;
            options.script = option.value("FILE");
            ++i;
            break;
        } else {
            throw unknown_option(arg);
        }
    }
    options.args.assign(argv.begin() + static_cast<std::ptrdiff_t>(i), argv.end());
    check_combinations(options);
    return options;
}

std::string read_file(const fs::path& path, std::error_code& ec) {
    ec.clear();
    const hx::UniqueFd fd(open(path.c_str(), O_RDONLY | O_CLOEXEC));
    std::string text;
    if (!fd.valid() || hx::read_all(fd.get(), text) != hx::ReadAll::Done) {
        ec.assign(errno, std::generic_category());
        return {};
    }
    return text;
}

int init_project(bool full) {
    if (fs::exists(kProjectLoop)) {
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

bool looks_secret(std::string name) {
    std::transform(name.begin(), name.end(), name.begin(),
                   [](unsigned char c) { return static_cast<char>(std::toupper(c)); });
    for (const std::string_view mark : {"KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIALS"}) {
        if (name.ends_with(mark)) {
            return true;
        }
    }
    return name.ends_with("_PAT") || name.ends_with("_PWD");
}

using Secrets = std::map<std::string, std::string, std::less<>>;

Secrets take_secrets(const std::vector<std::string>& key_envs, const std::vector<std::string>& keep) {
    Secrets secrets;
    std::vector<std::pair<std::string, char*>> doomed;
    // NOLINTNEXTLINE(cppcoreguidelines-pro-bounds-pointer-arithmetic)
    for (char* const* entry = environ; *entry != nullptr; ++entry) {
        const std::string_view text(*entry);
        const std::size_t equals = text.find('=');
        if (equals == std::string_view::npos) {
            continue;
        }
        const std::string name(text.substr(0, equals));
        const bool is_key = std::ranges::find(key_envs, name) != key_envs.end();
        if (is_key) {
            secrets[name] = text.substr(equals + 1);
        }
        const bool kept = std::ranges::find(keep, name) != keep.end();
        if (is_key || (looks_secret(name) && !kept)) {
            doomed.emplace_back(name, *entry);
        }
    }
    for (const auto& [name, entry] : doomed) {
        unsetenv(name.c_str());  // NOLINT(concurrency-mt-unsafe)
        explicit_bzero(entry, std::strlen(entry));
    }
    return secrets;
}

struct ModelChoice {
    std::string model;
    std::string url;
    std::string key_env;
    const Vendor* vendor;
};

ModelChoice choose_endpoint(const std::optional<std::string>& flag, bool rpi) {
    std::string model = rpi ? std::string(kRpiLead) : env_or("HX_MODEL", "");
    if (flag) {
        model = *flag;
    }
    for (const auto& [alias, name] : kModelAliases) {
        if (model == alias) {
            model = name;
        }
    }
    if (model.empty()) {
        const bool only_anthropic = env_or(kDeepSeek.key_env, "").empty() && !env_or(kAnthropic.key_env, "").empty();
        model = only_anthropic && env_or("HX_API_URL", "").empty() ? kAnthropic.model : kDeepSeek.model;
    }
    const Vendor& vendor = vendor_of(model);
    return {
        .model = model,
        .url = env_or("HX_API_URL", vendor.url),
        .key_env = env_or("HX_KEY_ENV", vendor.key_env),
        .vendor = &vendor,
    };
}

class TempDir {
  public:
    TempDir() {
        std::error_code ec;
        std::string pattern = (fs::temp_directory_path(ec) / "hx-XXXXXX").string();
        if (ec || mkdtemp(pattern.data()) == nullptr) {
            throw UsageError(fmt::format("cannot create a temporary directory: {}", hx::error_text()));
        }
        path_ = std::move(pattern);
        setenv("TMPDIR", path_.c_str(), 1);  // NOLINT(concurrency-mt-unsafe)
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

bool holds_home(const fs::path& project) {
    std::error_code ec;
    const fs::path home = fs::canonical(env_or("HOME", ""), ec);
    if (ec || home.empty()) {
        return false;
    }
    return hx::beneath(home, project);
}

void add_path_rules(std::vector<hx::Rule>& rules) {
    std::vector<fs::path> readable;
    for (const hx::Rule& rule : rules) {
        std::error_code ec;
        if (fs::path whole = fs::canonical(rule.path, ec); !ec) {
            readable.push_back(std::move(whole));
        }
    }
    std::set<fs::path> opened;
    const auto open = [&](const fs::path& dir) {
        std::error_code ec;
        const fs::path whole = fs::canonical(dir, ec);
        const bool covered = std::ranges::any_of(readable, [&](const fs::path& r) { return hx::beneath(whole, r); });
        if (!ec && fs::is_directory(whole, ec) && whole != "/" && !holds_home(whole) && !covered) {
            opened.insert(whole);
        }
    };
    for (const auto part : std::views::split(env_or("PATH", ""), ':')) {
        if (const std::string_view entry(part.begin(), part.end()); entry.starts_with('/')) {
            open(entry);
        }
    }
    const std::set<fs::path> listed = opened;
    for (const fs::path& dir : listed) {
        std::error_code ec;
        for (fs::directory_iterator it(dir, ec), end; !ec && it != end; it.increment(ec)) {
            std::error_code link_ec;
            if (!it->is_symlink(link_ec)) {
                continue;
            }
            const fs::path home = fs::canonical(it->path(), link_ec).parent_path();
            if (link_ec || listed.contains(home)) {
                continue;
            }
            open(home);
            if (home.filename() == "bin") {
                for (const char* sibling : {"lib", "lib64", "libexec"}) {
                    open(home.parent_path() / sibling);
                }
            }
        }
    }
    for (const fs::path& dir : opened) {
        rules.push_back({.path = dir.string(), .access = hx::Access::Read, .required = false});
    }
}

std::vector<hx::Rule> system_rules() {
    using hx::Access;
    std::vector<hx::Rule> rules;
    for (const char* path : {
             "/usr",
             "/bin",
             "/sbin",
             "/lib",
             "/lib32",
             "/lib64",
             "/libx32",
             "/etc",
             "/opt",
             "/proc",
             "/sys",
             "/nix/store",
             "/run/systemd/resolve",
         }) {
        rules.push_back({.path = path, .access = Access::Read, .required = false});
    }
    for (const char* path : {"/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom"}) {
        rules.push_back({.path = path, .access = Access::Device, .required = false});
    }
    const std::string home = env_or("HOME", "");
    if (home.starts_with('/')) {
        rules.push_back({.path = home + "/.gitconfig", .access = Access::Read, .required = false});
    }
    std::string config = env_or("XDG_CONFIG_HOME", "");
    if (!config.starts_with('/') && home.starts_with('/')) {
        config = home + "/.config";
    }
    if (config.starts_with('/')) {
        for (const char* name : {"config", "ignore", "attributes"}) {
            rules.push_back(
                {.path = fmt::format("{}/git/{}", config, name), .access = Access::Read, .required = false});
        }
    }
    for (const char* name : {
             "SSL_CERT_FILE",
             "SSL_CERT_DIR",
             "CURL_CA_BUNDLE",
             "REQUESTS_CA_BUNDLE",
             "NODE_EXTRA_CA_CERTS",
             "GIT_SSL_CAINFO",
             "CARGO_HTTP_CAINFO",
             "AWS_CA_BUNDLE",
             "NIX_SSL_CERT_FILE",
         }) {
        if (std::string path = env_or(name, ""); path.starts_with('/')) {
            rules.push_back({.path = std::move(path), .access = Access::Read, .required = false});
        }
    }
    add_path_rules(rules);
    return rules;
}

std::vector<hx::Rule> user_rules(const Options& options) {
    std::vector<hx::Rule> rules;
    rules.reserve(options.read_paths.size() + options.write_paths.size());
    for (const std::string& path : options.read_paths) {
        rules.push_back({.path = path, .access = hx::Access::Read, .required = true});
    }
    for (const std::string& path : options.write_paths) {
        rules.push_back({.path = path, .access = hx::Access::Write, .required = true});
    }
    return rules;
}

std::vector<hx::Rule> sandbox_rules(const Options& options, const fs::path& project, const fs::path& tmp) {
    using hx::Access;
    std::vector<hx::Rule> rules = system_rules();
    rules.push_back({.path = project.string(), .access = Access::Write, .required = true});
    rules.push_back({.path = tmp.string(), .access = Access::Write, .required = true});
    const std::vector<hx::Rule> user = user_rules(options);
    rules.insert(rules.end(), user.begin(), user.end());
    return rules;
}

void check_sandbox_paths(const Options& options, const fs::path& project) {
    if (project == "/" || holds_home(project)) {
        throw UsageError(
            fmt::format("refusing to treat {} as a project: the sandbox would leave your home directory open. "
                        "Run hx inside a project directory.",
                        project.string()));
    }
    for (const auto& [flag, paths] :
         {std::pair{"--ro", &options.read_paths}, std::pair{"--rw", &options.write_paths}}) {
        for (const std::string& path : *paths) {
            std::error_code ec;
            const fs::path whole = fs::canonical(path, ec);
            if (!ec && (whole == "/" || holds_home(whole))) {
                throw UsageError(
                    fmt::format("refusing {} {}: it would open your whole home directory. Name the "
                                "directories inside it that the task needs, or pass --no-sandbox.",
                                flag, path));
            }
        }
    }
}

hx::Sandbox confine(const Options& options, const fs::path& project, const fs::path& tmp) {
    check_sandbox_paths(options, project);
    try {
        return hx::Sandbox::confine(sandbox_rules(options, project, tmp), options.unix_sockets);
    } catch (const hx::SandboxError& error) {
        throw UsageError(fmt::format("no sandbox: {}. Pass --no-sandbox to run without one.", error.what()));
    }
}

constexpr std::size_t kMaxVmMemory = std::size_t{1} << 30;
constexpr std::size_t kVmStep = std::size_t{64} << 20;
constexpr std::size_t kSystemReserve = std::size_t{128} << 20;

std::optional<std::size_t> system_available() noexcept {
    const hx::UniqueFd file(open("/proc/meminfo", O_RDONLY | O_CLOEXEC));
    std::array<char, 4096> buffer{};
    const ssize_t n = file.valid() ? read(file.get(), buffer.data(), buffer.size()) : -1;
    if (n <= 0) {
        return std::nullopt;
    }
    const std::string_view text(buffer.data(), static_cast<std::size_t>(n));
    constexpr std::string_view kField = "MemAvailable:";
    std::size_t at = text.find(kField);
    if (at == std::string_view::npos) {
        return std::nullopt;
    }
    at = text.find_first_not_of(' ', at + kField.size());
    if (at == std::string_view::npos) {
        return std::nullopt;
    }
    std::size_t kib = 0;
    const auto [end, error] = std::from_chars(text.data() + at, text.data() + text.size(), kib);
    if (error != std::errc()) {
        return std::nullopt;
    }
    return kib * 1024;
}

struct VmMemory {
    std::size_t used = 0;
    std::size_t granted = 0;
};

bool grant(VmMemory& memory, std::size_t growth) noexcept {
    const std::size_t need = memory.used + growth;
    if (need <= memory.granted) {
        return true;
    }
    if (need > kMaxVmMemory) {
        return false;
    }
    const std::optional<std::size_t> available = system_available();
    if (!available) {
        memory.granted = kMaxVmMemory;
        return true;
    }
    const std::size_t room = *available > kSystemReserve ? *available - kSystemReserve : 0;
    if (growth > room) {
        return false;
    }
    memory.granted = std::min(kMaxVmMemory, memory.used + std::min(room, std::max(growth, kVmStep)));
    return true;
}

// NOLINTBEGIN(cppcoreguidelines-no-malloc)
void* limited_alloc(void* user, void* block, std::size_t old_size, std::size_t new_size) {
    auto& memory = *static_cast<VmMemory*>(user);
    std::size_t& used = memory.used;
    if (new_size == 0) {
        std::free(block);
        used -= old_size;
        memory.granted = std::min(memory.granted, used + kVmStep);
        return nullptr;
    }
    if (new_size > old_size && !grant(memory, new_size - old_size)) {
        hx::vm_memory_refused();
        return nullptr;
    }
    void* const grown = std::realloc(block, new_size);  // NOLINT(misc-const-correctness)
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

bool is_stop(lua_State* L) {
    const char* text = lua_tostring(L, -1);
    const std::string_view message = text != nullptr ? text : "";
    return message == "interrupted" || message == "terminated";
}

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

bool is_module(lua_State* L) {
    if (!lua_istable(L, -1)) {
        return false;
    }
    lua_getfield(L, -1, "main");
    const bool module = lua_isfunction(L, -1);
    lua_pop(L, 1);
    return module;
}

int run_main(lua_State* L) {
    lua_getfield(L, -1, "main");
    if (lua_pcall(L, 0, 1, 0) != 0) {
        return failure(L);
    }
    return lua_isnumber(L, -1) != 0 ? lua_tointeger(L, -1) : 0;
}

int run_module(lua_State* L, std::string_view source, const std::string& name) {
    if (!load_chunk(L, source, name) || lua_pcall(L, 0, 1, 0) != 0) {
        return failure(L);
    }
    return is_module(L) ? run_main(L) : 0;
}

int run_loop(lua_State* L, bool built_in) {
    std::error_code ec;
    if (built_in) {
        return run_module(L, hx::kDefaultLoop, "=loop");
    }
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
        fmt::print(stderr, "hx: {} {}. Fix it, or pass --built-in-loop to run without it.\n", kProjectLoop, why);
        return kExitUsage;
    }
    return run_module(L, hx::kDefaultLoop, "=loop");
}

void freeze_libraries(lua_State* L) {
    lua_pushnil(L);
    while (lua_next(L, LUA_GLOBALSINDEX) != 0) {
        if (lua_istable(L, -1) && lua_rawequal(L, -1, LUA_GLOBALSINDEX) == 0) {
            lua_setreadonly(L, -1, 1);
        }
        lua_pop(L, 1);
    }
    lua_pushliteral(L, "");
    if (lua_getmetatable(L, -1) != 0) {
        lua_setreadonly(L, -1, 1);
        lua_pop(L, 1);
    }
    lua_pop(L, 1);
}

template <typename Body>
int run_vm(hx::HostConfig config, const hx::Sandbox& sandbox, const Body& body) {
    hx::install_signal_handlers();
    hx::Host host(std::move(config), sandbox);
    VmMemory vm_memory;
    const LuaState state(lua_newstate(limited_alloc, &vm_memory));
    if (!state) {
        throw std::runtime_error("cannot create a Luau state");
    }
    lua_State* L = state.get();
    luaL_openlibs(L);
    hx::open_json(L);
    freeze_libraries(L);
    host.open(L);
    return body(L);
}

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

void close_inherited_fds() {
    if (hx::cloexec_above_stderr() != 0) {
        throw std::runtime_error(fmt::format("cannot mark inherited descriptors close-on-exec: {}", hx::error_text()));
    }
}

void check_effort() {
    if (const std::string effort = env_or("HX_EFFORT", ""); effort.empty()) {
        unsetenv("HX_EFFORT");  // NOLINT(concurrency-mt-unsafe)
    } else if (std::ranges::find(kEfforts, effort) == std::end(kEfforts)) {
        throw UsageError(fmt::format("HX_EFFORT={}: the efforts are low, medium, high, xhigh and max", effort));
    }
}

std::string service_url(const Service& service) {
    std::string url = env_or(service.url_env, service.url);
    while (*service.path != '\0' && url.ends_with('/')) {
        url.pop_back();
    }
    return url + service.path;
}

std::vector<std::string> key_envs(const ModelChoice& model) {
    std::vector<std::string> names{model.key_env};
    for (const Vendor& vendor : kVendors) {
        names.emplace_back(vendor.key_env);
    }
    for (const Service& service : kServices) {
        names.emplace_back(service.key_env);
    }
    return names;
}

std::vector<hx::Endpoint> endpoints(const ModelChoice& model, const Secrets& secrets) {
    const auto key = [&](std::string_view name) {
        const auto found = secrets.find(name);
        return found != secrets.end() ? found->second : std::string();
    };
    std::vector<hx::Endpoint> endpoints;
    endpoints.push_back({
        .name = std::string(hx::kSessionEndpoint),
        .url = model.url,
        .auth = hx::Auth::ApiKey,
        .key = key(model.key_env),
        .key_env = "",
        .headers = {model.vendor->version},
    });
    for (const Vendor& vendor : kVendors) {
        endpoints.push_back({
            .name = std::string(vendor.name),
            .url = env_or(vendor.url_env, vendor.url),
            .auth = hx::Auth::ApiKey,
            .key = key(vendor.key_env),
            .key_env = vendor.key_env,
            .headers = {vendor.version},
        });
    }
    for (const Service& service : kServices) {
        endpoints.push_back({
            .name = service.name,
            .url = service_url(service),
            .auth = service.auth,
            .key = key(service.key_env),
            .key_env = service.key_env,
            .headers = {},
        });
    }
    return endpoints;
}

std::string ca_bundle() {
    for (const char* name : {"SSL_CERT_FILE", "CURL_CA_BUNDLE"}) {
        if (std::string path = env_or(name, ""); !path.empty() && access(path.c_str(), R_OK) == 0) {
            return path;
        }
    }
    return {};
}

hx::HostConfig host_config(const Options& options, const ModelChoice& model, bool interactive) {
    hx::HostConfig config;
    config.endpoints = endpoints(model, take_secrets(key_envs(model), options.keep_env));
    config.model = model.model;
    config.args = options.args;
    config.interactive = interactive;
    config.quiet = options.quiet;
    config.default_loop = hx::kDefaultLoop;
    for (const auto& [name, source] : hx::kExtensions) {
        config.extensions.emplace_back(name, source);
    }
    for (const auto& [name, text] : hx::kSkills) {
        config.skills.emplace_back(name, text);
    }
    config.ca_bundle = ca_bundle();
    if (options.rpi) {
        config.workflow = kRpiWorkflow;
    }
    config.resume = options.resume.value_or("");
    return config;
}

std::optional<std::string> read_script(const Options& options) {
    if (options.mode != Mode::Script) {
        return std::nullopt;
    }
    if (options.script == "-") {
        if (isatty(STDIN_FILENO) != 0) {
            throw UsageError("--run - reads the script from stdin, which is a terminal; pipe the script in");
        }
        return std::string(std::istreambuf_iterator<char>(std::cin), std::istreambuf_iterator<char>());
    }
    std::error_code ec;
    std::string script = read_file(options.script, ec);
    if (ec) {
        throw UsageError(fmt::format("cannot read {}: {}", options.script, ec.message()));
    }
    return script;
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

    const ModelChoice endpoint = choose_endpoint(options.model, options.rpi);
    setenv("HX_MODEL", endpoint.model.c_str(), 1);  // NOLINT(concurrency-mt-unsafe)
    check_effort();
    const bool stdin_tty = isatty(STDIN_FILENO) != 0;
    if (options.no_input && options.mode == Mode::Agent && options.args.empty() && stdin_tty) {
        throw UsageError("no task, and --no-input forbids the prompt: give the task as arguments or on stdin");
    }
    hx::HostConfig config = host_config(
        options, endpoint, options.mode == Mode::Agent && options.args.empty() && stdin_tty && !options.no_input);
    const std::optional<std::string> script = read_script(options);

    std::error_code ec;
    const fs::path project = fs::canonical(fs::current_path(ec), ec);
    if (ec) {
        throw UsageError(fmt::format("cannot resolve the current directory: {}", ec.message()));
    }
    const TempDir tmp;

    static_cast<void>(std::setvbuf(stdout, nullptr, _IOLBF, 0));
    if (options.judge) {
        check_sandbox_paths(options, project);
    }
    std::optional<hx::Judge> judge =
        options.judge ? std::optional(spawn_judge(options, *options.judge, project, tmp.path())) : std::nullopt;
    const CurlGlobal curl;
    const hx::Sandbox sandbox = options.sandbox ? confine(options, project, tmp.path()) : hx::Sandbox::none();
    if (sandbox.confined() && !sandbox.blocks_unix_sockets()) {
        fmt::print(stderr,
                   "hx: note: this kernel's Landlock (ABI {}) cannot stop commands from connecting to unix sockets "
                   "such as the D-Bus session bus or ssh-agent; ABI {} can. See docs/wall.md.\n",
                   sandbox.abi(), hx::Sandbox::kUnixSocketAbi);
    }
    prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
    config.system_rules = system_rules();
    if (judge) {
        config.judge = judge->share_channel();
    }
    return run_vm(std::move(config), sandbox, [&](lua_State* L) {
        const int status =
            script ? run_module(L, *script, fmt::format("={}", options.script)) : run_loop(L, options.built_in_loop);
        if (judge) {
            judge->finish();
        }
        return status;
    });
}

int report(const char* message, int status) noexcept {
    static_cast<void>(std::fputs("hx: ", stderr));
    static_cast<void>(std::fputs(message, stderr));
    static_cast<void>(std::fputc('\n', stderr));
    return status;
}

}  // namespace

int main(int argc, char** argv) {
    try {
        // NOLINTNEXTLINE(cppcoreguidelines-pro-bounds-pointer-arithmetic)
        return run(std::vector<std::string_view>(argv + std::min(argc, 1), argv + argc));
    } catch (const UsageError& error) {
        return report(error.what(), kExitUsage);
    } catch (const std::exception& error) {
        return report(error.what(), kExitFailure);
    } catch (...) {
        return report("unknown error", kExitFailure);
    }
}
