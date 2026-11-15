// host.sh: running a command behind the wall, waiting for it within its limits, and keeping both ends of its output.

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

constexpr std::size_t kCaptureHalf = std::size_t{512} << 10;  // host.sh keeps this much of each end of the output
constexpr double kNoPidfdPoll = 0.05;  // seconds between looks at a command whose exit cannot be waited for

// Keeps the first and the last part of a stream and counts what fell out of the middle: the start of a command's
// output says what ran, the end says how it failed.
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

    // At most kCaptureHalf from each end. Each part ends or starts at a line where one is near, and never inside
    // a character: half a character is not valid UTF-8, and would be lost without a word.
    [[nodiscard]] std::string text() const {
        if (head_.size() + tail_.size() <= 2 * kCaptureHalf) {
            return head_ + tail_;  // nothing was dropped: the tail only fills once the head is full
        }
        const auto continues = [](char c) { return (static_cast<unsigned char>(c) & 0xC0U) == 0x80U; };
        std::string_view head = head_;
        std::string_view tail = std::string_view(tail_).substr(tail_.size() - kCaptureHalf);
        if (const std::size_t line = head.rfind('\n'); line != std::string_view::npos && line > head.size() / 2) {
            head = head.substr(0, line + 1);
        } else {
            while (!head.empty() && continues(head.back())) {
                head.remove_suffix(1);
            }
            if (!head.empty() && static_cast<unsigned char>(head.back()) >= 0xC0U) {
                head.remove_suffix(1);  // the lead byte of the character that was cut
            }
        }
        if (const std::size_t line = tail.find('\n'); line != std::string_view::npos && line < tail.size() / 2) {
            tail.remove_prefix(line + 1);
        } else {
            while (!tail.empty() && continues(tail.front())) {
                tail.remove_prefix(1);
            }
        }
        const std::size_t omitted = dropped_ + head_.size() + tail_.size() - head.size() - tail.size();
        return fmt::format("{}{}[... {} bytes of output omitted ...]\n{}", head, head.ends_with('\n') ? "" : "\n",
                           omitted, tail);
    }

  private:
    std::string head_;
    std::string tail_;
    std::size_t dropped_ = 0;
};

// The read end of the pipe a command writes its output to, and what has come through it.
class Output {
  public:
    explicit Output(UniqueFd pipe) noexcept : pipe_(std::move(pipe)) { fcntl(pipe_.get(), F_SETFL, O_NONBLOCK); }

    // Reads what the pipe holds now, without waiting for more.
    void drain() {
        std::array<char, kBufferSize> buffer{};
        while (!at_end_) {
            const ssize_t n = read(pipe_.get(), buffer.data(), buffer.size());
            if (n > 0) {
                capture_.add({buffer.data(), static_cast<std::size_t>(n)});
            } else if (n == 0) {
                at_end_ = true;
            } else if (errno != EINTR) {
                break;  // EAGAIN: nothing more for now
            }
        }
    }

    [[nodiscard]] int fd() const noexcept { return pipe_.get(); }
    // Every writer has closed the pipe.
    [[nodiscard]] bool at_end() const noexcept { return at_end_; }
    [[nodiscard]] std::string text() const { return capture_.text(); }

  private:
    UniqueFd pipe_;
    Capture capture_;
    bool at_end_ = false;
};

// The command is running until it exits by itself, runs out of time, a signal arrives, or stdin's writer hangs up.
// All but the first kill its whole process group; a signal then leaves through raise_if_signaled.
enum class Ending : std::uint8_t { Exited, TimedOut, Signaled, HungUp };

struct Outcome {
    Ending ending = Ending::Exited;
    int status = 0;      // as waitpid gives it
    bool reaped = true;  // false: killed, but stuck in uninterruptible I/O and not yet dead
    double ended = 0;    // when it exited, or was given up on, on the monotonic clock
};

void kill_group(pid_t group) {
    // Until the child has called setsid() its group does not exist yet; then the child itself is the target.
    if (kill(-group, SIGKILL) != 0) {
        kill(group, SIGKILL);
    }
}

// Waits for a killed child for at most seconds. SIGKILL cannot end a process stuck in uninterruptible I/O (a dead
// network mount) until the I/O returns, so it may take longer; it is then left a zombie for a later reap, by the
// next host.sh or by kill_descendants.
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

// Runs in the forked child, so it may only call async-signal-safe functions: no allocation, no stdio.
[[noreturn]] void exec_bash(const char* command, int output, const Wall* wall) {
    // A session of its own: one process group to kill, and no terminal to ask a question on.
    setsid();
    if (wall != nullptr && !wall->enter()) {
        constexpr std::string_view message = "hx: cannot put up the command's wall\n";
        if (write(output, message.data(), message.size()) < 0) {
            // Nothing more can be said.
        }
        _exit(126);
    }
    const int null = open("/dev/null", O_RDONLY);
    if (null > STDIN_FILENO) {
        dup2(null, STDIN_FILENO);
        close(null);  // only the copy on stdin is for the command
    }
    dup2(output, STDOUT_FILENO);
    dup2(output, STDERR_FILENO);
    // Nothing else hx holds is for the command: libcurl keeps the connection it reuses, and its wake-up pipe, open
    // between requests, and does not make them close-on-exec. If this fails, the exec still goes ahead; main
    // checked at the start that it works.
    static_cast<void>(cloexec_above_stderr());
    for (const int sig : {SIGINT, SIGTERM, SIGHUP, SIGPIPE}) {
        static_cast<void>(std::signal(sig, SIG_DFL));
    }
    execlp("bash", "bash", "-c", command, static_cast<char*>(nullptr));
    constexpr std::string_view message = "hx: cannot run bash\n";
    if (write(STDERR_FILENO, message.data(), message.size()) < 0) {
        // Nothing more can be said.
    }
    _exit(127);
}

// A command that has started: bash's PID, the read end of the pipe it writes its output to, and when it was forked.
struct Child {
    pid_t pid;
    UniqueFd output;
    double started;
};

// Forks bash to run command, behind wall if there is one.
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

// Gathers the command's output until it exits, the time runs out, a signal arrives, or (with hangup) stdin's writer
// hangs up; in all but the first it is killed.
Outcome wait_for_command(pid_t pid, Output& output, bool hangup, double timeout) {
    // Readable when bash exits, so its end is seen at once, not at the next poll. Linux 5.3; without it, polling.
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
        // Wake on bash's exit, a hang-up, output, a signal or the deadline. A negative descriptor is not watched.
        std::array<pollfd, 3> waiters{{{exited.get(), POLLIN, 0}, {hangup_fd, POLLRDHUP, 0}, {output.fd(), POLLIN, 0}}};
        const double left = std::max(deadline - now(), 0.0);
        wait_for(std::span(waiters).first(output.at_end() ? 2 : 3),
                 exited.valid() ? left : std::min(left, kNoPidfdPoll));
        if ((waiters[1].revents & (POLLRDHUP | POLLHUP)) != 0) {
            return give_up(Ending::HungUp);
        }
    }
}

// Whether stdin's writer has hung up already.
bool hung_up() {
    pollfd already{STDIN_FILENO, POLLRDHUP, 0};
    return poll(&already, 1, 0) > 0 && (already.revents & (POLLRDHUP | POLLHUP)) != 0;
}

// Pushes host.sh's result: {out, code or signal, timed_out, seconds}.
void push_result(lua_State* L, const std::string& text, const Outcome& outcome, double seconds) {
    lua_createtable(L, 0, 5);
    lua_pushlstring(L, text.data(), text.size());
    lua_setfield(L, -2, "out");
    if (!outcome.reaped) {
        lua_pushnumber(L, SIGKILL);  // what it was sent and will die of once its I/O returns
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
    bool on_hangup = false;  // {hangup = true}: the command ends when stdin's writer hangs up
    if (lua_istable(L, 2)) {
        lua_getfield(L, 2, "hangup");
        on_hangup = lua_toboolean(L, -1) != 0;
        lua_pop(L, 1);
    }
    // A command behind a narrower wall is not trusted to leave anything running: what it started dies with it.
    const std::vector<pid_t> before = wall ? descendants_now() : std::vector<pid_t>{};
    raise_if_signaled(L);  // a stop in progress starts no command
    if (on_hangup && hung_up()) {
        raise_error(L, "hung up");
    }
    // Adopted orphans that have exited are zombies of hx. No command of hx's own is running now, so this cannot
    // take the status of one; a judge that has exited is reaped here too, which finish_judge allows for.
    reap_exited();

    Child child = start(L, command, wall);
    Output output(std::move(child.output));
    const Outcome outcome = wait_for_command(child.pid, output, on_hangup, timeout);
    // bash is gone, or killed and left for a later reap. Whatever it left running keeps running, so do not wait
    // for the pipe to close; unless it ran behind a narrower wall, and then what it left is killed now.
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

// The wall that host.sh's options read and write ask for, or none when they ask for nothing.
std::optional<Wall> Host::narrower_wall(lua_State* L, int idx) const {
    if (!lua_istable(L, idx)) {
        return std::nullopt;
    }
    std::vector<Rule> rules = config_.system_rules;
    bool asked = false;  // an empty list still asks: the command then reaches the system directories only
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
