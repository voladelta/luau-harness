// host.spawn and host.wait: children of hx that each run a Luau function in a copy of the VM, at the same time as
// hx and as each other. The loop's agents are these.

#include <fcntl.h>
#include <poll.h>
#include <sys/prctl.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <iostream>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "host.h"
#include "host_internal.h"
#include "lua.h"
#include "lualib.h"
#include "sys.h"

namespace hx {
namespace {

constexpr std::size_t kMaxSpawned = 64;                     // children running, or ended and not yet waited for
constexpr std::size_t kMaxResult = std::size_t{256} << 20;  // a larger result ends its child
constexpr double kWaitSlack = 2.0;  // host.wait returns this long before the host.timed limit, as host.judge does

// What a child writes to its pipe as it ends: whether its function returned or raised, the length of the text it
// returned or raised, and the text. A child that was killed leaves less than that, or nothing.
constexpr char kReturned = '+';
constexpr char kRaised = '-';
constexpr std::size_t kHeaderSize = 1 + sizeof(std::uint64_t);

std::string frame(bool returned, std::string_view text) {
    const std::uint64_t length = text.size();
    std::string out(kHeaderSize, returned ? kReturned : kRaised);
    std::memcpy(out.data() + 1, &length, sizeof(length));
    out.append(text);
    return out;
}

// The text of a whole frame and whether its function returned; nothing for a frame that was cut short.
bool unframe(std::string_view data, bool& returned, std::string_view& text) {
    if (data.size() < kHeaderSize || (data.front() != kReturned && data.front() != kRaised)) {
        return false;
    }
    std::uint64_t length = 0;
    std::memcpy(&length, data.data() + 1, sizeof(length));
    if (length != data.size() - kHeaderSize) {
        return false;
    }
    returned = data.front() == kReturned;
    text = data.substr(kHeaderSize);
    return true;
}

// What the function left on top of the stack, as the text the parent gets, and whether that counts as a return.
// A function returns a string or nothing; an error is whatever was raised.
std::string result_text(lua_State* L, bool& returned) {
    if (returned && lua_isnoneornil(L, -1)) {
        return {};
    }
    if (lua_type(L, -1) == LUA_TSTRING) {
        std::size_t length = 0;
        const char* text = lua_tolstring(L, -1, &length);
        return {text, length};
    }
    const bool was_returned = std::exchange(returned, false);
    return was_returned ? std::string("host.spawn: the function returned a ") + luaL_typename(L, -1) +
                              ", where a string or nothing is what goes back"
                        : std::string("(error is not a string)");
}

}  // namespace

// The pipe's end is the child's: only the child holds the other end, so the pipe ends when the child does.
void Host::drain(Spawned& child) {
    std::array<char, kBufferSize> buffer{};
    while (!child.ended) {
        const ssize_t n = read(child.result.get(), buffer.data(), buffer.size());
        if (n > 0 && child.data.size() + static_cast<std::size_t>(n) <= kMaxResult) {
            child.data.append(buffer.data(), static_cast<std::size_t>(n));
        } else if (n >= 0) {
            // The end, or more than a result may hold: then the pipe is closed, the child's next write fails and
            // ends it, and what came is no whole frame.
            child.result.reset();
            child.ended = true;
        } else if (errno != EINTR) {
            break;  // EAGAIN: nothing more for now
        }
    }
}

// The child's side of host.spawn: runs the function on the stack with its arguments, sends back what it returned
// or raised, and leaves. It never returns into the parent's frames, whose destructors (the temporary directory's,
// the Host's) are the parent's to run.
void Host::run_child(lua_State* L, int result_fd) noexcept {
    int status = 1;
    try {
        child_ = true;
        spawned_.clear();       // its siblings are the parent's to wait for
        config_.judge.reset();  // and the judge is the parent's to talk to
        // The parent's time limit is the parent's, which waits for the child or does not. A stop is the child's
        // too, and ends it: its whole run counts as one host.try that never returns, so no host.try inside the
        // function is the outermost, where a stop would end and the function go on.
        deadline_ = 0;
        ++tries_;
        renew_limit_timer();
        // An orphan of the child's commands comes to the child, as the parent's come to the parent, so the child
        // can kill what it started.
        prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        // Stdin is the parent's to read: the child reads nothing, not even what the parent had already buffered.
        if (const int null = ::open("/dev/null", O_RDONLY | O_CLOEXEC); null >= 0) {
            dup2(null, STDIN_FILENO);
            close(null);
        }
        std::cin.ignore(std::cin.rdbuf()->in_avail());

        bool returned = lua_pcall(L, lua_gettop(L) - 1, 1, 0) == 0;
        const std::string text = result_text(L, returned);
        kill_descendants();  // what the child's commands left running ends with it
        static_cast<void>(std::fflush(nullptr));
        if (write_all(result_fd, frame(returned, text)) && returned) {
            status = 0;
        }
    } catch (...) {
        status = 1;  // the parent reads that no result came
    }
    _exit(status);
}

// host.spawn(fn, ...) -> id: forks hx. The child is this process as it is now, VM and wall and all, and runs
// fn(...) while the parent goes on; host.wait returns what fn returned. What the child changes in the VM is its
// own: only files and its result reach the parent.
int Host::spawn(lua_State* L) {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    Host& host = of(L);
    if (host.child_) {
        luaL_error(L, "host.spawn: a spawned child cannot spawn");
    }
    if (host.spawned_.size() >= kMaxSpawned) {
        luaL_error(L,
                   "host.spawn: %d children are running or not yet waited for, which is the most; host.wait "
                   "takes one off the list",
                   static_cast<int>(kMaxSpawned));
    }
    raise_if_signaled(L);  // a stop in progress starts no child
    std::array<int, 2> ends{};
    if (pipe2(ends.data(), O_CLOEXEC) != 0) {
        luaL_error(L, "pipe: %s", error_text().c_str());
    }
    UniqueFd reader(ends[0]);
    const UniqueFd writer(ends[1]);
    // The connection libcurl keeps open between requests would be one TLS session in two processes. It is closed
    // here, and each process opens its own with its next request.
    host.curl_.reset();
    static_cast<void>(std::fflush(nullptr));  // or the child would write out the parent's buffers again
    const pid_t pid = fork();
    if (pid < 0) {
        luaL_error(L, "fork: %s", error_text().c_str());
    }
    if (pid == 0) {
        reader.reset();
        host.run_child(L, writer.get());
    }
    fcntl(reader.get(), F_SETFL, O_NONBLOCK);
    const int id = ++host.spawn_count_;
    host.spawned_.push_back({.id = id, .result = std::move(reader), .data = {}, .ended = false});
    lua_pushinteger(L, id);
    return 1;
}

// host.wait(seconds?) -> id, ok, text | nil, running: the first child that has ended, in the order they were
// spawned: its id, whether its function returned, and what it returned or raised. A child that was killed, or
// ended in any other way without a result, is `false` and says so. When no child has ended it waits for one, for at
// most `seconds` and never past kWaitSlack before the host.timed limit in force, so that the caller can still say
// what it was waiting for; then, or when there is no child, it returns nil and how many are still running. A stop
// raises, and nothing is lost: the children are still there to wait for.
int Host::wait(lua_State* L) {
    Host& host = of(L);
    const bool limited = !lua_isnoneornil(L, 1);
    double until = limited ? now() + host.within_limit(luaL_checknumber(L, 1)) : 0;  // 0: as long as it takes
    if (host.deadline_ > 0) {
        until = until > 0 ? std::min(until, host.deadline_ - kWaitSlack) : host.deadline_ - kWaitSlack;
    }
    for (;;) {
        raise_if_signaled(L);
        for (Spawned& child : host.spawned_) {
            drain(child);
        }
        if (const auto done = std::ranges::find(host.spawned_, true, &Spawned::ended); done != host.spawned_.end()) {
            const Spawned child = std::move(*done);
            host.spawned_.erase(done);
            reap_exited();
            bool returned = false;
            std::string_view text = "the child ended without a result: it was killed, or ran out of memory";
            static_cast<void>(unframe(child.data, returned, text));
            lua_pushinteger(L, child.id);
            lua_pushboolean(L, returned ? 1 : 0);
            lua_pushlstring(L, text.data(), text.size());
            return 3;
        }
        if (host.spawned_.empty() || (until > 0 && now() >= until)) {
            lua_pushnil(L);
            lua_pushinteger(L, static_cast<int>(host.spawned_.size()));
            return 2;
        }
        std::vector<pollfd> waiters;
        waiters.reserve(host.spawned_.size());
        for (const Spawned& child : host.spawned_) {
            waiters.push_back({child.result.get(), POLLIN, 0});
        }
        wait_for(waiters, until > 0 ? std::max(until - now(), 0.0) : -1.0);
    }
}

}  // namespace hx
