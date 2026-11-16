#include <fcntl.h>
#include <poll.h>
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
#include <optional>
#include <span>
#include <string>
#include <string_view>
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

constexpr std::size_t kCaptureHalf = std::size_t{512} << 10;
constexpr double kNoPidfdPoll = 0.05;

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

    [[nodiscard]] std::string text() const {
        if (head_.size() + tail_.size() <= 2 * kCaptureHalf) {
            return head_ + tail_;
        }
        const std::string_view head = cut_head(head_);
        const std::string_view tail = cut_tail(std::string_view(tail_).substr(tail_.size() - kCaptureHalf));
        const std::size_t omitted = dropped_ + head_.size() + tail_.size() - head.size() - tail.size();
        return fmt::format("{}{}[... {} bytes of output omitted ...]\n{}", head, head.ends_with('\n') ? "" : "\n",
                           omitted, tail);
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
    [[nodiscard]] std::string text() const { return capture_.text(); }

  private:
    UniqueFd pipe_;
    Capture capture_;
    bool at_end_ = false;
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

[[noreturn]] void exec_bash(const char* command, int output, const Wall* wall) {
    setsid();
    if (wall != nullptr && !wall->enter()) {
        constexpr std::string_view message = "hx: cannot put up the command's wall\n";
        if (write(output, message.data(), message.size()) < 0) {
        }
        _exit(126);
    }
    const int null = open("/dev/null", O_RDONLY);
    if (null > STDIN_FILENO) {
        dup2(null, STDIN_FILENO);
        close(null);
    }
    dup2(output, STDOUT_FILENO);
    dup2(output, STDERR_FILENO);
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

struct Child {
    pid_t pid;
    UniqueFd output;
    double started;
};

Child start(lua_State* L, const char* command, const std::optional<Wall>& wall) {
    std::array<int, 2> pipe_fds{};
    if (pipe2(pipe_fds.data(), O_CLOEXEC) != 0) {
        luaL_error(L, "pipe: %s", error_text().c_str());
    }
    UniqueFd output(pipe_fds[0]);
    const UniqueFd input(pipe_fds[1]);
    const double started = now();
    const pid_t pid = fork();
    if (pid < 0) {
        luaL_error(L, "fork: %s", error_text().c_str());
    }
    if (pid == 0) {
        exec_bash(command, input.get(), wall ? &*wall : nullptr);
    }
    return {.pid = pid, .output = std::move(output), .started = started};
}

Outcome wait_for_command(pid_t pid, Output& output, bool hangup, double timeout) {
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
        output.drain();
        if (waitpid(pid, &outcome.status, WNOHANG) == pid) {
            outcome.ended = now();
            return outcome;
        }
        if (g_signal != 0 || now() >= deadline) {
            return give_up(g_signal != 0 ? Ending::Signaled : Ending::TimedOut);
        }
        std::array<pollfd, 3> waiters{{{exited.get(), POLLIN, 0}, {hangup_fd, POLLRDHUP, 0}, {output.fd(), POLLIN, 0}}};
        const double left = std::max(deadline - now(), 0.0);
        wait_for(std::span(waiters).first(output.at_end() ? 2 : 3),
                 exited.valid() ? left : std::min(left, kNoPidfdPoll));
        if ((waiters[1].revents & (POLLRDHUP | POLLHUP)) != 0) {
            return give_up(Ending::HungUp);
        }
    }
}

bool hung_up() {
    pollfd already{STDIN_FILENO, POLLRDHUP, 0};
    return poll(&already, 1, 0) > 0 && (already.revents & (POLLRDHUP | POLLHUP)) != 0;
}

void push_result(lua_State* L, const std::string& text, const Outcome& outcome, double seconds) {
    lua_createtable(L, 0, 5);
    lua_pushlstring(L, text.data(), text.size());
    lua_setfield(L, -2, "out");
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

}  // namespace

int Host::sh(lua_State* L) {
    const char* command = check_cstring(L, 1);
    const Host& host = of(L);
    const double timeout = host.within_limit(option(L, 2, "timeout", 120));
    const std::optional<Wall> wall = host.narrower_wall(L, 2);
    bool on_hangup = false;
    if (lua_istable(L, 2)) {
        lua_getfield(L, 2, "hangup");
        on_hangup = lua_toboolean(L, -1) != 0;
        lua_pop(L, 1);
    }
    const std::vector<pid_t> before = wall ? descendants_now() : std::vector<pid_t>{};
    raise_if_signaled(L);
    if (on_hangup && hung_up()) {
        raise_error(L, "hung up");
    }
    reap_exited();

    Child child = start(L, command, wall);
    Output output(std::move(child.output));
    const Outcome outcome = wait_for_command(child.pid, output, on_hangup, timeout);
    if (wall) {
        kill_descendants(before);
    }
    output.drain();
    if (outcome.ending == Ending::Signaled) {
        raise_if_signaled(L);
    }
    if (outcome.ending == Ending::HungUp) {
        raise_error(L, "hung up");
    }
    push_result(L, output.text(), outcome, outcome.ended - child.started);
    return 1;
}

std::optional<Wall> Host::narrower_wall(lua_State* L, int idx) const {
    if (!lua_istable(L, idx)) {
        return std::nullopt;
    }
    std::vector<Rule> rules = config_.system_rules;
    bool asked = false;
    for (const auto& [field, access] : {std::pair{"read", Access::Read}, std::pair{"write", Access::Write}}) {
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
                rules.push_back({path, access, true});
                lua_pop(L, 1);
            }
            lua_pop(L, 1);
        }
        lua_pop(L, 1);
    }
    if (!asked) {
        return std::nullopt;
    }
    try {
        return Wall::prepare(rules);
    } catch (const SandboxError& error) {
        luaL_error(L, "host.sh: %s", error.what());
    }
}

}  // namespace hx
