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

[[noreturn]] void exec_bash(const char* command, Streams streams, const Wall* wall, Place place) {
    setsid();
    if (wall != nullptr && !wall->enter()) {
        constexpr std::string_view message = "hx: cannot put up the command's wall\n";
        if (write(streams.err, message.data(), message.size()) < 0) {
        }
        _exit(126);
    }
    if (place.cwd != nullptr && chdir(place.cwd) != 0) {
        constexpr std::string_view message = "hx: cannot enter the command's cwd\n";
        if (write(streams.err, message.data(), message.size()) < 0) {
        }
        _exit(126);
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
    constexpr std::string_view message = "hx: cannot run bash\n";
    if (write(STDERR_FILENO, message.data(), message.size()) < 0) {
    }
    _exit(127);
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

std::pair<UniqueFd, UniqueFd> make_pipe(lua_State* L) {
    std::array<int, 2> fds{};
    if (pipe2(fds.data(), O_CLOEXEC) != 0) {
        luaL_error(L, "pipe: %s", error_text().c_str());
    }
    return {UniqueFd(fds[0]), UniqueFd(fds[1])};
}

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

struct Pipes {
    Output& output;
    Output* errors;
    Input& input;

    void drain() const {
        output.drain();
        if (errors != nullptr) {
            errors->drain();
        }
    }
};

Outcome wait_for_command(pid_t pid, Pipes pipes, bool hangup, double timeout) {
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
        if (g_signal != 0 || now() >= deadline) {
            return give_up(g_signal != 0 ? Ending::Signaled : Ending::TimedOut);
        }
        const auto readable = [](const Output* stream) {
            return stream == nullptr || stream->at_end() ? -1 : stream->fd();
        };
        std::array<pollfd, 5> waiters{
            pollfd{.fd = exited.get(), .events = POLLIN, .revents = 0},
            pollfd{.fd = hangup_fd, .events = POLLRDHUP, .revents = 0},
            pollfd{.fd = readable(&pipes.output), .events = POLLIN, .revents = 0},
            pollfd{.fd = pipes.input.fd(), .events = POLLOUT, .revents = 0},
            pollfd{.fd = readable(pipes.errors), .events = POLLIN, .revents = 0},
        };
        const double left = std::max(deadline - now(), 0.0);
        wait_for(waiters, exited.valid() ? left : std::min(left, kNoPidfdPoll));
        if ((waiters[1].revents & (POLLRDHUP | POLLHUP)) != 0) {
            return give_up(Ending::HungUp);
        }
        if (waiters[3].revents != 0) {
            pipes.input.feed();
        }
    }
}

std::optional<std::string> cwd_of(lua_State* L, int idx) {
    lua_getfield(L, idx, "cwd");
    if (lua_isnil(L, -1)) {
        lua_pop(L, 1);
        return std::nullopt;
    }
    std::size_t length = 0;
    const char* path = lua_type(L, -1) == LUA_TSTRING ? lua_tolstring(L, -1, &length) : nullptr;
    if (path == nullptr || std::strlen(path) != length) {
        luaL_error(L, "host.sh: cwd is the path of a directory");
    }
    std::string cwd(path, length);
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
        std::size_t name_length = 0;
        const char* name = lua_type(L, -2) == LUA_TSTRING ? lua_tolstring(L, -2, &name_length) : nullptr;
        if (name == nullptr || name_length == 0 || std::strlen(name) != name_length ||
            std::strchr(name, '=') != nullptr) {
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
    lua_pushlstring(L, output.text.data(), output.text.size());
    lua_setfield(L, -2, "out");
    if (output.omitted > 0) {
        lua_pushnumber(L, static_cast<double>(output.omitted));
        lua_setfield(L, -2, "omitted");
    }
    if (errors) {
        lua_pushlstring(L, errors->text.data(), errors->text.size());
        lua_setfield(L, -2, "err");
        if (errors->omitted > 0) {
            lua_pushnumber(L, static_cast<double>(errors->omitted));
            lua_setfield(L, -2, "err_omitted");
        }
    }
    if (!outcome.reaped) {
        lua_pushnumber(L, SIGKILL);
        lua_setfield(L, -2, "signal");
    } else if (WIFEXITED(outcome.status)) {
        lua_pushnumber(L, WEXITSTATUS(outcome.status));
        lua_setfield(L, -2, "code");
    } else if (WIFSIGNALED(outcome.status)) {
        lua_pushnumber(L, WTERMSIG(outcome.status));
        lua_setfield(L, -2, "signal");
    }
    lua_pushboolean(L, outcome.ending == Ending::TimedOut ? 1 : 0);
    lua_setfield(L, -2, "timed_out");
    lua_pushnumber(L, seconds);
    lua_setfield(L, -2, "seconds");
}

std::optional<Wall> narrower_wall(lua_State* L, int idx, const std::vector<Rule>& system_rules) {
    if (!lua_istable(L, idx)) {
        return std::nullopt;
    }
    std::vector<Rule> rules = system_rules;
    std::vector<fs::path> inert;
    bool asked = false;
    for (const auto& [field, access] :
         {std::pair{"read", std::optional(Access::Read)}, std::pair{"write", std::optional(Access::Write)},
          std::pair{"inert", std::optional<Access>()}}) {
        lua_getfield(L, idx, field);
        if (!lua_isnil(L, -1)) {
            if (!lua_istable(L, -1)) {
                luaL_error(L, "host.sh: %s is a list of paths", field);
            }
            asked = true;
            for (int i = 1; lua_rawgeti(L, -1, i) != LUA_TNIL; ++i) {
                std::size_t length = 0;
                const char* path = lua_type(L, -1) == LUA_TSTRING ? lua_tolstring(L, -1, &length) : nullptr;
                if (path == nullptr || std::strlen(path) != length) {
                    luaL_error(L, "host.sh: %s[%d] is not a path", field, i);
                }
                if (access) {
                    rules.push_back({.path = path, .access = *access, .required = true});
                } else {
                    std::error_code ec;
                    inert.push_back(fs::weakly_canonical(path, ec));
                    if (ec) {
                        luaL_error(L, "host.sh: inert[%d]: %s", i, ec.message().c_str());
                    }
                }
                lua_pop(L, 1);
            }
            lua_pop(L, 1);
        }
        lua_pop(L, 1);
    }
    if (!asked) {
        return std::nullopt;
    }
    for (Rule& rule : rules) {
        std::error_code ec;
        const fs::path whole = inert.empty() ? fs::path() : fs::weakly_canonical(rule.path, ec);
        rule.inert = ec || std::ranges::any_of(inert, [&whole](const fs::path& dir) {
                         return beneath(whole, dir) || beneath(dir, whole);
                     });
    }
    try {
        return Wall::prepare(rules);
    } catch (const SandboxError& error) {
        luaL_error(L, "host.sh: %s", error.what());
    }
}

}  // namespace

int run_sh(lua_State* L, double deadline, const std::vector<Rule>& system_rules) {
    const std::string command = command_of(L);
    const double timeout = within_limit(deadline, option(L, 2, "timeout", 120));
    const std::optional<Wall> wall = narrower_wall(L, 2, system_rules);
    bool on_hangup = false;
    bool split = false;
    std::optional<std::string_view> data;
    std::optional<std::string> cwd;
    std::vector<std::string> entries;
    if (lua_istable(L, 2)) {
        cwd = cwd_of(L, 2);
        entries = environment_of(L, 2);
        lua_getfield(L, 2, "hangup");
        on_hangup = lua_toboolean(L, -1) != 0;
        lua_pop(L, 1);
        lua_getfield(L, 2, "split");
        split = lua_toboolean(L, -1) != 0;
        lua_pop(L, 1);
        lua_getfield(L, 2, "input");
        if (!lua_isnil(L, -1)) {
            std::size_t length = 0;
            const char* text = lua_isstring(L, -1) != 0 ? lua_tolstring(L, -1, &length) : nullptr;
            if (text == nullptr) {
                luaL_error(L, "host.sh: input is the text to send to the command's stdin");
            }
            data = std::string_view(text, length);
        }
    }
    const std::vector<pid_t> before = wall ? descendants_now() : std::vector<pid_t>{};
    raise_if_signaled(L);
    if (on_hangup && hung_up()) {
        raise_error(L, "hung up");
    }
    reap_exited();

    std::vector<char*> environment;
    for (std::string& entry : entries) {
        if (!entry.empty()) {
            environment.push_back(entry.data());
        }
    }
    environment.push_back(nullptr);
    const Place place{
        .cwd = cwd ? cwd->c_str() : nullptr,
        .environment = entries.empty() ? nullptr : environment.data(),
    };
    Child child = start(L, command.c_str(), {.input = data.has_value(), .split = split}, wall, place);
    Output output(std::move(child.output));
    std::optional<Output> errors;
    if (split) {
        errors.emplace(std::move(child.errors));
    }
    Input input(std::move(child.input), data.value_or(std::string_view{}));
    const Pipes pipes{.output = output, .errors = errors ? &*errors : nullptr, .input = input};
    const Outcome outcome = wait_for_command(child.pid, pipes, on_hangup, timeout);
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
    push_result(L, output.result(), errors ? std::optional<Captured>(errors->result()) : std::nullopt, outcome,
                outcome.ended - child.started);
    return 1;
}

}  // namespace hx
