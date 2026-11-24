#include <fcntl.h>
#include <poll.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <csignal>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <optional>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <fmt/format.h>

#include "host.h"
#include "host_internal.h"
#include "lua.h"
#include "lualib.h"
#include "sandbox.h"
#include "sys.h"

namespace hx {
namespace {

namespace fs = std::filesystem;

constexpr std::size_t kCaptureHalf = std::size_t{8} << 20;
constexpr double kNoPidfdPoll = 0.05;

struct Captured {
    std::string text;
    std::size_t omitted;
};

class Capture {
  public:
    void add(std::string_view data) {
        if (head_.size() < kCaptureHalf) {
            const std::size_t take = std::min(data.size(), kCaptureHalf - head_.size());
            head_.append(data.substr(0, take));
            data.remove_prefix(take);
        }
        if (data.empty()) {
            return;
        }
        tail_.append(data);
        if (tail_.size() > 2 * kCaptureHalf) {
            const std::size_t cut = tail_.size() - kCaptureHalf;
            dropped_ += cut;
            tail_.erase(0, cut);
        }
    }

    [[nodiscard]] Captured result() const {
        if (dropped_ == 0 && head_.size() + tail_.size() <= 2 * kCaptureHalf) {
            return {.text = head_ + tail_, .omitted = 0};
        }
        const std::string_view head = cut_head(head_);
        const std::string_view tail = cut_tail(std::string_view(tail_).substr(tail_.size() - kCaptureHalf));
        const std::size_t omitted = dropped_ + head_.size() + tail_.size() - head.size() - tail.size();
        return {
            .text = fmt::format("{}{}[... {} bytes of output omitted ...]\n{}", head, head.ends_with('\n') ? "" : "\n",
                                omitted, tail),
            .omitted = omitted,
        };
    }

  private:
    std::string head_;
    std::string tail_;
    std::size_t dropped_ = 0;
};

class Output {
  public:
    explicit Output(UniqueFd pipe) noexcept : pipe_(std::move(pipe)) { fcntl(pipe_.get(), F_SETFL, O_NONBLOCK); }

    void drain() {
        std::array<char, kBufferSize> buffer{};
        while (!at_end_) {
            const ssize_t n = read(pipe_.get(), buffer.data(), buffer.size());
            if (n > 0) {
                capture_.add({buffer.data(), static_cast<std::size_t>(n)});
            } else if (n == 0) {
                at_end_ = true;
            } else if (errno != EINTR) {
                break;
            }
        }
    }

    [[nodiscard]] int fd() const noexcept { return pipe_.get(); }
    [[nodiscard]] bool at_end() const noexcept { return at_end_; }
    [[nodiscard]] Captured result() const { return capture_.result(); }

  private:
    UniqueFd pipe_;
    Capture capture_;
    bool at_end_ = false;
};

class Input {
  public:
    Input(UniqueFd pipe, std::string_view data) noexcept : pipe_(std::move(pipe)), rest_(data) {
        if (pipe_.valid()) {
            fcntl(pipe_.get(), F_SETFL, O_NONBLOCK);
            feed();
        }
    }

    void feed() noexcept {
        while (pipe_.valid() && !rest_.empty()) {
            const ssize_t n = write(pipe_.get(), rest_.data(), rest_.size());
            if (n > 0) {
                rest_.remove_prefix(static_cast<std::size_t>(n));
            } else if (n < 0 && errno == EAGAIN) {
                return;
            } else if (n < 0 && errno != EINTR) {
                break;
            }
        }
        pipe_.reset();
    }

    [[nodiscard]] int fd() const noexcept { return pipe_.get(); }

  private:
    UniqueFd pipe_;
    std::string_view rest_;
};

enum class Ending : std::uint8_t { Exited, TimedOut, Signaled, HungUp };

struct Outcome {
    Ending ending = Ending::Exited;
    int status = 0;
    bool reaped = true;
    double ended = 0;
};

void kill_group(pid_t group) {
    if (kill(-group, SIGKILL) != 0) {
        kill(group, SIGKILL);
    }
}

bool reap_within(pid_t pid, int& status, double seconds) {
    const double deadline = now() + seconds;
    for (;;) {
        const pid_t done = waitpid(pid, &status, WNOHANG);
        if (done == pid) {
            return true;
        }
        if ((done < 0 && errno != EINTR) || now() >= deadline) {
            return false;
        }
        nap(10);
    }
}

struct Place {
    const char* cwd = nullptr;
    char** environment = nullptr;
};

struct Streams {
    int in = -1;
    int out = -1;
    int err = -1;
};

[[noreturn]] void quit(int fd, std::string_view message, int status) {
    if (write(fd, message.data(), message.size()) < 0) {
    }
    _exit(status);
}

[[noreturn]] void exec_bash(const char* command, Streams streams, const Wall* wall, Place place) {
    setsid();
    if (wall != nullptr && !wall->enter()) {
        quit(streams.err, "hx: cannot put up the command's wall\n", 126);
    }
    if (place.cwd != nullptr && chdir(place.cwd) != 0) {
        quit(streams.err, "hx: cannot enter the command's cwd\n", 126);
    }
    if (place.environment != nullptr) {
        environ = place.environment;
    }
    const int in = streams.in >= 0 ? streams.in : open("/dev/null", O_RDONLY);
    if (in > STDIN_FILENO) {
        dup2(in, STDIN_FILENO);
    }
    dup2(streams.out, STDOUT_FILENO);
    dup2(streams.err, STDERR_FILENO);
    static_cast<void>(cloexec_above_stderr());
    for (const int sig : {SIGINT, SIGTERM, SIGHUP, SIGPIPE}) {
        static_cast<void>(std::signal(sig, SIG_DFL));
    }
    execlp("bash", "bash", "-c", command, static_cast<char*>(nullptr));
    quit(STDERR_FILENO, "hx: cannot run bash\n", 127);
}

struct Wiring {
    bool input = false;
    bool split = false;
};

struct Child {
    pid_t pid;
    UniqueFd output;
    UniqueFd errors;
    UniqueFd input;
    double started;
};

Child start(lua_State* L, const char* command, Wiring wiring, const std::optional<Wall>& wall, Place place) {
    auto [output, output_end] = make_pipe(L);
    auto [errors, errors_end] = wiring.split ? make_pipe(L) : std::pair<UniqueFd, UniqueFd>{};
    auto [input_end, input] = wiring.input ? make_pipe(L) : std::pair<UniqueFd, UniqueFd>{};
    const double started = now();
    const pid_t pid = fork();
    if (pid < 0) {
        luaL_error(L, "fork: %s", error_text().c_str());
    }
    if (pid == 0) {
        const Streams streams{
            .in = input_end.get(),
            .out = output_end.get(),
            .err = wiring.split ? errors_end.get() : output_end.get(),
        };
        exec_bash(command, streams, wall ? &*wall : nullptr, place);
    }
    return {
        .pid = pid,
        .output = std::move(output),
        .errors = std::move(errors),
        .input = std::move(input),
        .started = started,
    };
}

std::string quote(std::string_view word) {
    std::string quoted = "'";
    for (const char c : word) {
        quoted += c == '\'' ? std::string_view("'\\''") : std::string_view(&c, 1);
    }
    return quoted + "'";
}

std::string command_of(lua_State* L) {
    if (!lua_istable(L, 1)) {
        return check_cstring(L, 1);
    }
    std::string command;
    for (int i = 1; lua_rawgeti(L, 1, i) != LUA_TNIL; ++i) {
        std::size_t length = 0;
        const char* word = lua_isstring(L, -1) != 0 ? lua_tolstring(L, -1, &length) : nullptr;
        if (word == nullptr) {
            luaL_error(L, "host.sh: command[%d] is a %s, not a string", i, luaL_typename(L, -1));
        }
        if (std::strlen(word) != length) {
            luaL_error(L, "host.sh: command[%d] contains a NUL byte", i);
        }
        if (i > 1) {
            command += ' ';
        }
        command += quote({word, length});
        lua_pop(L, 1);
    }
    lua_pop(L, 1);
    if (command.empty()) {
        luaL_error(L, "host.sh: the command list is empty; give the program and its arguments");
    }
    return command;
}

class Pipes {
  public:
    Pipes(UniqueFd output, UniqueFd errors, UniqueFd input, std::string_view data)
        : output_(std::move(output)),
          errors_(errors.valid() ? std::optional<Output>(std::in_place, std::move(errors)) : std::nullopt),
          input_(std::move(input), data) {}

    void drain() {
        output_.drain();
        if (errors_) {
            errors_->drain();
        }
    }

    void feed() noexcept { input_.feed(); }

    [[nodiscard]] int output_fd() const noexcept { return readable(output_); }
    [[nodiscard]] int errors_fd() const noexcept { return errors_ ? readable(*errors_) : -1; }
    [[nodiscard]] int input_fd() const noexcept { return input_.fd(); }

    [[nodiscard]] Captured output() const { return output_.result(); }
    [[nodiscard]] std::optional<Captured> errors() const {
        return errors_ ? std::optional<Captured>(errors_->result()) : std::nullopt;
    }

  private:
    static int readable(const Output& stream) noexcept { return stream.at_end() ? -1 : stream.fd(); }

    Output output_;
    std::optional<Output> errors_;
    Input input_;
};

Outcome wait_for_command(pid_t pid, Pipes& pipes, bool hangup, double timeout) {
    const UniqueFd exited(static_cast<int>(syscall(SYS_pidfd_open, pid, 0)));
    const int hangup_fd = hangup ? STDIN_FILENO : -1;
    const double deadline = now() + timeout;
    Outcome outcome;
    const auto give_up = [&](Ending ending) {
        outcome.ending = ending;
        kill_group(pid);
        outcome.reaped = reap_within(pid, outcome.status, kReapSeconds);
        outcome.ended = now();
        return outcome;
    };
    for (;;) {
        pipes.drain();
        if (waitpid(pid, &outcome.status, WNOHANG) == pid) {
            outcome.ended = now();
            return outcome;
        }
        if (const bool signaled = pending_signal() != 0; signaled || now() >= deadline) {
            return give_up(signaled ? Ending::Signaled : Ending::TimedOut);
        }
        std::array<pollfd, 5> waiters{
            pollfd{.fd = exited.get(), .events = POLLIN, .revents = 0},
            pollfd{.fd = hangup_fd, .events = POLLRDHUP, .revents = 0},
            pollfd{.fd = pipes.output_fd(), .events = POLLIN, .revents = 0},
            pollfd{.fd = pipes.input_fd(), .events = POLLOUT, .revents = 0},
            pollfd{.fd = pipes.errors_fd(), .events = POLLIN, .revents = 0},
        };
        const double left = std::max(deadline - now(), 0.0);
        wait_for(waiters, exited.valid() ? left : std::min(left, kNoPidfdPoll));
        if ((waiters[1].revents & (POLLRDHUP | POLLHUP)) != 0) {
            return give_up(Ending::HungUp);
        }
        if (waiters[3].revents != 0) {
            pipes.feed();
        }
    }
}

const char* cstring_at(lua_State* L, int idx) {
    std::size_t length = 0;
    const char* text = lua_type(L, idx) == LUA_TSTRING ? lua_tolstring(L, idx, &length) : nullptr;
    return text != nullptr && std::strlen(text) == length ? text : nullptr;
}

std::optional<std::string> cwd_of(lua_State* L, int idx) {
    lua_getfield(L, idx, "cwd");
    if (lua_isnil(L, -1)) {
        lua_pop(L, 1);
        return std::nullopt;
    }
    const char* path = cstring_at(L, -1);
    if (path == nullptr) {
        luaL_error(L, "host.sh: cwd is the path of a directory");
    }
    std::string cwd(path);
    lua_pop(L, 1);
    struct stat st{};
    if (stat(cwd.c_str(), &st) != 0) {
        luaL_error(L, "host.sh: cwd %s: %s", cwd.c_str(), error_text().c_str());
    }
    if (!S_ISDIR(st.st_mode)) {
        luaL_error(L, "host.sh: cwd %s is not a directory", cwd.c_str());
    }
    return cwd;
}

std::vector<std::string> environment_of(lua_State* L, int idx) {
    lua_getfield(L, idx, "env");
    if (lua_isnil(L, -1)) {
        lua_pop(L, 1);
        return {};
    }
    if (!lua_istable(L, -1)) {
        luaL_error(L, "host.sh: env is a table of names and values");
    }
    std::vector<std::pair<std::string, std::optional<std::string>>> changes;
    lua_pushnil(L);
    while (lua_next(L, -2) != 0) {
        const char* name = cstring_at(L, -2);
        if (name == nullptr || *name == '\0' || std::strchr(name, '=') != nullptr) {
            luaL_error(L, "host.sh: env has a name that is not a variable's");
        }
        std::size_t length = 0;
        const char* value =
            lua_type(L, -1) == LUA_TSTRING || lua_type(L, -1) == LUA_TNUMBER ? lua_tolstring(L, -1, &length) : nullptr;
        const bool removed = lua_type(L, -1) == LUA_TBOOLEAN && lua_toboolean(L, -1) == 0;
        if (value == nullptr && !removed) {
            luaL_error(L, "host.sh: env.%s is text, or false to remove it", name);
        }
        if (value != nullptr && std::strlen(value) != length) {
            luaL_error(L, "host.sh: env.%s contains a NUL byte", name);
        }
        changes.emplace_back(name, value == nullptr ? std::nullopt : std::optional<std::string>(value));
        lua_pop(L, 1);
    }
    lua_pop(L, 1);
    std::vector<std::string> entries;
    for (char* const* each = environ; *each != nullptr; ++each) {
        const std::string_view entry(*each);
        const std::string_view name = entry.substr(0, entry.find('='));
        if (std::none_of(changes.begin(), changes.end(), [&](const auto& change) { return change.first == name; })) {
            entries.emplace_back(entry);
        }
    }
    for (const auto& [name, value] : changes) {
        if (value) {
            entries.push_back(name + "=" + *value);
        }
    }
    if (entries.empty()) {
        entries.emplace_back();
    }
    return entries;
}

bool hung_up() {
    pollfd already{.fd = STDIN_FILENO, .events = POLLRDHUP, .revents = 0};
    return poll(&already, 1, 0) > 0 && (already.revents & (POLLRDHUP | POLLHUP)) != 0;
}

void push_result(lua_State* L, const Captured& output, const std::optional<Captured>& errors, const Outcome& outcome,
                 double seconds) {
    lua_createtable(L, 0, 8);
    set_text(L, "out", output.text);
    if (output.omitted > 0) {
        set_number(L, "omitted", static_cast<double>(output.omitted));
    }
    if (errors) {
        set_text(L, "err", errors->text);
        if (errors->omitted > 0) {
            set_number(L, "err_omitted", static_cast<double>(errors->omitted));
        }
    }
    if (!outcome.reaped) {
        set_number(L, "signal", SIGKILL);
    } else if (WIFEXITED(outcome.status)) {
        set_number(L, "code", WEXITSTATUS(outcome.status));
    } else if (WIFSIGNALED(outcome.status)) {
        set_number(L, "signal", WTERMSIG(outcome.status));
    }
    set_flag(L, "timed_out", outcome.ending == Ending::TimedOut);
    set_number(L, "seconds", seconds);
}

template <typename Fn>
bool for_each_path(lua_State* L, int idx, const char* field, const Fn& fn) {
    lua_getfield(L, idx, field);
    const bool given = !lua_isnil(L, -1);
    if (given) {
        if (!lua_istable(L, -1)) {
            luaL_error(L, "host.sh: %s is a list of paths", field);
        }
        for (int i = 1; lua_rawgeti(L, -1, i) != LUA_TNIL; ++i) {
            const char* path = cstring_at(L, -1);
            if (path == nullptr) {
                luaL_error(L, "host.sh: %s[%d] is not a path", field, i);
            }
            fn(i, path);
            lua_pop(L, 1);
        }
        lua_pop(L, 1);
    }
    lua_pop(L, 1);
    return given;
}

bool overlap(const fs::path& one, const fs::path& other) {
    return beneath(one, other) || beneath(other, one);
}

std::optional<Wall> narrower_wall(lua_State* L, int idx, const std::vector<Rule>& system_rules) {
    if (!lua_istable(L, idx)) {
        return std::nullopt;
    }
    std::vector<Rule> rules = system_rules;
    std::vector<fs::path> inert;
    const auto allow = [&rules](Access access) {
        return [&rules, access](int /*i*/, const char* path) {
            rules.push_back({.path = path, .access = access, .required = true});
        };
    };
    const bool reads = for_each_path(L, idx, "read", allow(Access::Read));
    const bool writes = for_each_path(L, idx, "write", allow(Access::Write));
    const bool inerts = for_each_path(L, idx, "inert", [&](int i, const char* path) {
        std::error_code ec;
        inert.push_back(fs::weakly_canonical(path, ec));
        if (ec) {
            luaL_error(L, "host.sh: inert[%d]: %s", i, ec.message().c_str());
        }
    });
    if (!reads && !writes && !inerts) {
        return std::nullopt;
    }
    for (Rule& rule : rules) {
        std::error_code ec;
        const fs::path whole = inert.empty() ? fs::path() : fs::weakly_canonical(rule.path, ec);
        rule.inert = ec || std::ranges::any_of(inert, [&whole](const fs::path& dir) { return overlap(whole, dir); });
    }
    try {
        return Wall::prepare(rules);
    } catch (const SandboxError& error) {
        luaL_error(L, "host.sh: %s", error.what());
    }
}

struct Options {
    std::optional<std::string> cwd;
    std::vector<std::string> environment;
    std::optional<std::string_view> input;
    bool hangup = false;
    bool split = false;
};

bool flag(lua_State* L, int idx, const char* name) {
    lua_getfield(L, idx, name);
    const bool set = lua_toboolean(L, -1) != 0;
    lua_pop(L, 1);
    return set;
}

std::optional<std::string_view> anchored_input(lua_State* L, int idx) {
    lua_getfield(L, idx, "input");
    if (lua_isnil(L, -1)) {
        return std::nullopt;
    }
    std::size_t length = 0;
    const char* text = lua_isstring(L, -1) != 0 ? lua_tolstring(L, -1, &length) : nullptr;
    if (text == nullptr) {
        luaL_error(L, "host.sh: input is the text to send to the command's stdin");
    }
    return std::string_view(text, length);
}

Options options_of(lua_State* L, int idx) {
    Options options;
    if (lua_istable(L, idx)) {
        options.cwd = cwd_of(L, idx);
        options.environment = environment_of(L, idx);
        options.hangup = flag(L, idx, "hangup");
        options.split = flag(L, idx, "split");
        options.input = anchored_input(L, idx);
    }
    return options;
}

std::vector<char*> pointers_to(std::vector<std::string>& entries) {
    std::vector<char*> pointers;
    for (std::string& entry : entries) {
        if (!entry.empty()) {
            pointers.push_back(entry.data());
        }
    }
    pointers.push_back(nullptr);
    return pointers;
}

}  // namespace

int run_sh(lua_State* L, double deadline, const std::vector<Rule>& system_rules) {
    const std::string command = command_of(L);
    const double timeout = within_limit(deadline, option(L, 2, "timeout", 120));
    const std::optional<Wall> wall = narrower_wall(L, 2, system_rules);
    Options options = options_of(L, 2);
    const std::vector<pid_t> before = wall ? descendants_now() : std::vector<pid_t>{};
    raise_if_signaled(L);
    if (options.hangup && hung_up()) {
        raise_error(L, "hung up");
    }
    reap_exited();

    std::vector<char*> environment = pointers_to(options.environment);
    const Place place{
        .cwd = options.cwd ? options.cwd->c_str() : nullptr,
        .environment = options.environment.empty() ? nullptr : environment.data(),
    };
    const Wiring wiring{.input = options.input.has_value(), .split = options.split};
    Child child = start(L, command.c_str(), wiring, wall, place);
    Pipes pipes(std::move(child.output), std::move(child.errors), std::move(child.input),
                options.input.value_or(std::string_view{}));
    const Outcome outcome = wait_for_command(child.pid, pipes, options.hangup, timeout);
    if (wall) {
        kill_descendants(before);
    }
    pipes.drain();
    if (outcome.ending == Ending::Signaled) {
        raise_if_signaled(L);
    }
    if (outcome.ending == Ending::HungUp) {
        raise_error(L, "hung up");
    }
    push_result(L, pipes.output(), pipes.errors(), outcome, outcome.ended - child.started);
    return 1;
}

}  // namespace hx
