#include "cli.h"

#include <unistd.h>

#include <algorithm>
#include <cctype>
#include <cstddef>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <iterator>
#include <map>
#include <optional>
#include <ranges>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <fmt/format.h>

#include "embedded.h"
#include "sys.h"
#include "usage.h"
#include "vm.h"

namespace hx {
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
  hx --rpi [options] [TASK...]        run the agent as a lead, on Carbon: it researches and plans, agents
                                      on Hydrogen implement side by side, and it verifies their work

examples:
  hx "add a --json flag to the export command and test it"
  hx --rpi "move the config loader to TOML, with its tests and docs"
  { echo review this change; git diff; } | hx
  hx --ro ~/.rustup --ro ~/.cargo --rw ~/.cargo/registry "fix the failing test"
  hx -q "summarise the README" > summary.txt

options:
  --model NAME      model to ask (overrides HX_MODEL): carbon, argon, neon, hydrogen or a full name
  --judge KIT       judge the agent's changes with the kit in directory KIT (run, and optionally build
                    and check); the agent cannot read the kit, and the champion, its patch and the
                    ledger go to a directory outside the wall
  --rpi             lead the task by the rpi workflow (research, plan, implement): the model is carbon
                    unless --model names another, whatever HX_MODEL says, and its agents run on
                    gemini-4-hydrogen
  --endpoint NAME=URL
                    one more endpoint for host.post(NAME, body), for an extension of yours: its key is
                    read from NAME_API_KEY (my-api has MY_API_API_KEY) and sent as a Bearer token, to
                    that URL only
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
  HX_MODEL          model name; carbon, argon, neon and hydrogen are the Gemini 4 models (default
                    gemini-4-neon)
  HX_API_URL        the model endpoint, which speaks Gemini's Interactions API (default
                    https://generativelanguage.googleapis.com/v1beta/interactions)
  HX_KEY_ENV        name of the variable that holds the API key (default GEMINI_API_KEY)
  HX_EFFORT         the model's thinking level: minimal, low, medium, high or xhigh, where xhigh is
                    high on a model without it (default: the model's own)
  HX_EXA_URL        Exa's search endpoint, which the built-in web and exa extensions search through
                    with EXA_API_KEY (default https://api.exa.ai/search)
  HX_EXA_CONTENTS_URL
                    Exa's contents endpoint, which the built-in web extension reads pages through
                    with EXA_API_KEY (default https://api.exa.ai/contents)

output:
  The model's words go to stdout; progress, warnings and errors go to stderr.

exit status:
  0 done, 1 the run failed, 2 bad usage or hx cannot start here, 130 interrupted, 143 terminated

docs: https://github.com/voladelta/luau-harness
)";

constexpr const char* kModelUrl = "https://generativelanguage.googleapis.com/v1beta/interactions";
constexpr const char* kModelKeyEnv = "GEMINI_API_KEY";
constexpr const char* kModel = "gemini-4-neon";

struct Service {
    const char* name;
    const char* url;
    const char* url_env;
    const char* key_env;
    Auth auth;
};

constexpr Service kServices[] = {
    {
        .name = "exa",
        .url = "https://api.exa.ai/search",
        .url_env = "HX_EXA_URL",
        .key_env = "EXA_API_KEY",
        .auth = Auth::ApiKey,
    },
    {
        .name = "contents",
        .url = "https://api.exa.ai/contents",
        .url_env = "HX_EXA_CONTENTS_URL",
        .key_env = "EXA_API_KEY",
        .auth = Auth::ApiKey,
    },
};

constexpr std::pair<std::string_view, std::string_view> kModelAliases[] = {
    {"carbon", "gemini-4-carbon"},
    {"argon", "gemini-4-argon"},
    {"neon", "gemini-4-neon"},
    {"hydrogen", "gemini-4-hydrogen"},
};
constexpr std::string_view kRpiLead = "carbon";
constexpr const char* kRpiWorkflow = "rpi";
constexpr std::string_view kEfforts[] = {"minimal", "low", "medium", "high", "xhigh"};

constexpr std::string_view kOptionNames[] = {
    "--model",   "--ro",       "--rw",   "--keep-env",      "--unix-sockets", "--no-sandbox",
    "--quiet",   "--no-input", "--init", "--full",          "--run",          "--help",
    "--version", "--judge",    "--rpi",  "--built-in-loop", "--resume",       "--endpoint",
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

bool is_built_in_endpoint(std::string_view name) {
    return name == kSessionEndpoint || std::ranges::find(kServices, name, &Service::name) != std::end(kServices);
}

Endpoint named_endpoint(std::string_view pair, const std::vector<Endpoint>& taken) {
    const std::size_t equals = pair.find('=');
    const std::string_view name = pair.substr(0, equals);
    const std::string_view url = equals == std::string_view::npos ? std::string_view() : pair.substr(equals + 1);
    const bool well_named = equals != std::string_view::npos && !name.empty() && name.size() <= 40 &&
                            std::islower(static_cast<unsigned char>(name[0])) != 0 &&
                            std::ranges::all_of(name, [](unsigned char c) {
                                return std::islower(c) != 0 || std::isdigit(c) != 0 || c == '_' || c == '-';
                            });
    if (!well_named) {
        throw UsageError(
            fmt::format("--endpoint {}: it takes NAME=URL, and a name is up to 40 lower-case letters, digits, _ and -, "
                        "starting with a letter",
                        pair));
    }
    if (!url.starts_with("https://") && !url.starts_with("http://")) {
        throw UsageError(fmt::format("--endpoint {}: the URL starts with https:// or http://", pair));
    }
    if (is_built_in_endpoint(name) || std::ranges::find(taken, name, &Endpoint::name) != taken.end()) {
        throw UsageError(fmt::format("--endpoint {}: there is an endpoint named {} already", pair, name));
    }
    std::string key_env(name);
    std::ranges::transform(key_env, key_env.begin(),
                           [](unsigned char c) { return c == '-' ? '_' : static_cast<char>(std::toupper(c)); });
    return {
        .name = std::string(name),
        .url = std::string(url),
        .auth = Auth::Bearer,
        .key = "",
        .key_env = key_env + "_API_KEY",
    };
}

void check_combinations(const Options& options) {
    if (options.resume && options.mode != Mode::Agent) {
        throw UsageError("--resume goes on with the agent's conversation: it goes with neither --run nor --init");
    }
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
};

ModelChoice choose_model(const std::optional<std::string>& flag, bool rpi) {
    std::string model = rpi ? std::string(kRpiLead) : env_or("HX_MODEL", kModel);
    if (flag) {
        model = *flag;
    }
    for (const auto& [alias, name] : kModelAliases) {
        if (model == alias) {
            model = name;
        }
    }
    return {
        .model = model,
        .url = env_or("HX_API_URL", kModelUrl),
        .key_env = env_or("HX_KEY_ENV", kModelKeyEnv),
    };
}

void check_effort() {
    if (const std::string effort = env_or("HX_EFFORT", ""); effort.empty()) {
        unsetenv("HX_EFFORT");  // NOLINT(concurrency-mt-unsafe)
    } else if (std::ranges::find(kEfforts, effort) == std::end(kEfforts)) {
        throw UsageError(fmt::format("HX_EFFORT={}: the efforts are minimal, low, medium, high and xhigh", effort));
    }
}

std::vector<std::string> key_envs(const ModelChoice& model, const std::vector<Endpoint>& added) {
    std::vector<std::string> names{model.key_env, kModelKeyEnv};
    for (const Endpoint& endpoint : added) {
        names.emplace_back(endpoint.key_env);
    }
    for (const Service& service : kServices) {
        names.emplace_back(service.key_env);
    }
    return names;
}

std::vector<Endpoint> resolve_endpoints(const ModelChoice& model, const std::vector<Endpoint>& added,
                                        const Secrets& secrets) {
    const auto key = [&](std::string_view name) {
        const auto found = secrets.find(name);
        return found != secrets.end() ? found->second : std::string();
    };
    std::vector<Endpoint> list;
    list.push_back({
        .name = std::string(kSessionEndpoint),
        .url = model.url,
        .auth = Auth::GoogleKey,
        .key = key(model.key_env),
        .key_env = "",
    });
    for (const Service& service : kServices) {
        list.push_back({
            .name = service.name,
            .url = env_or(service.url_env, service.url),
            .auth = service.auth,
            .key = key(service.key_env),
            .key_env = service.key_env,
        });
    }
    for (Endpoint endpoint : added) {
        endpoint.key = key(endpoint.key_env);
        list.push_back(std::move(endpoint));
    }
    return list;
}

std::string ca_bundle() {
    for (const char* name : {"SSL_CERT_FILE", "CURL_CA_BUNDLE"}) {
        if (std::string path = env_or(name, ""); !path.empty() && access(path.c_str(), R_OK) == 0) {
            return path;
        }
    }
    return {};
}

}  // namespace

std::string_view usage() noexcept {
    return kUsage;
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
        } else if (arg == "--endpoint") {
            options.endpoints.push_back(named_endpoint(option.value("NAME=URL"), options.endpoints));
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
    out << (full ? kDefaultLoop : kLoopOverlay);
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

HostConfig build_host_config(const Options& options) {
    const ModelChoice model = choose_model(options.model, options.rpi);
    setenv("HX_MODEL", model.model.c_str(), 1);  // NOLINT(concurrency-mt-unsafe)
    check_effort();
    const bool stdin_tty = isatty(STDIN_FILENO) != 0;
    if (options.no_input && options.mode == Mode::Agent && options.args.empty() && stdin_tty) {
        throw UsageError("no task, and --no-input forbids the prompt: give the task as arguments or on stdin");
    }

    HostConfig config;
    config.endpoints =
        resolve_endpoints(model, options.endpoints, take_secrets(key_envs(model, options.endpoints), options.keep_env));
    config.model = model.model;
    config.args = options.args;
    config.interactive = options.mode == Mode::Agent && options.args.empty() && stdin_tty && !options.no_input;
    config.quiet = options.quiet;
    config.default_loop = kDefaultLoop;
    for (const auto& [name, source] : kExtensions) {
        config.extensions.emplace_back(name, source);
    }
    for (const auto& [name, text] : kSkills) {
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

}  // namespace hx
