#include <fcntl.h>
#include <poll.h>
#include <sys/prctl.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <csignal>
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

constexpr std::size_t kMaxSpawned = 64;
constexpr std::size_t kMaxResult = std::size_t{256} << 20;

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

void SpawnPool::drain(Spawned& child) {
    std::array<char, kBufferSize> buffer{};
    while (!child.ended) {
        const ssize_t n = read(child.result.get(), buffer.data(), buffer.size());
        if (n > 0 && child.data.size() + static_cast<std::size_t>(n) <= kMaxResult) {
            child.data.append(buffer.data(), static_cast<std::size_t>(n));
        } else if (n >= 0) {
            child.result.reset();
            child.ended = true;
        } else if (errno != EINTR) {
            break;
        }
    }
}

void SpawnPool::run_child(lua_State* L, int result_fd, const std::function<void()>& in_child) noexcept {
    int status = 1;
    try {
        child_ = true;
        spawned_.clear();
        in_child();
        renew_limit_timer();
        prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        [[maybe_unused]] FILE* const in = std::freopen("/dev/null", "r", stdin);
        std::cin.clear();

        bool returned = lua_pcall(L, lua_gettop(L) - 1, 1, 0) == 0;
        const std::string text = result_text(L, returned);
        kill_descendants();
        static_cast<void>(std::fflush(nullptr));
        if (write_all(result_fd, frame(returned, text)) && returned) {
            status = 0;
        }
    } catch (...) {
        status = 1;
    }
    _exit(status);
}

int SpawnPool::spawn(lua_State* L, const std::function<void()>& before_fork, const std::function<void()>& in_child) {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    if (child_) {
        luaL_error(L, "host.spawn: a spawned child cannot spawn");
    }
    if (spawned_.size() >= kMaxSpawned) {
        luaL_error(L,
                   "host.spawn: %d children are running or not yet waited for, which is the most; host.wait "
                   "takes one off the list",
                   static_cast<int>(kMaxSpawned));
    }
    raise_if_signaled(L);
    auto [reader, writer] = make_pipe(L);
    before_fork();
    static_cast<void>(std::fflush(nullptr));
    const pid_t pid = fork();
    if (pid < 0) {
        luaL_error(L, "fork: %s", error_text().c_str());
    }
    if (pid == 0) {
        reader.reset();
        run_child(L, writer.get(), in_child);
    }
    fcntl(reader.get(), F_SETFL, O_NONBLOCK);
    const int id = ++count_;
    spawned_.push_back({.id = id, .pid = pid, .result = std::move(reader), .data = {}, .ended = false});
    lua_pushinteger(L, id);
    return 1;
}

int SpawnPool::stop(lua_State* L) {
    const int id = luaL_checkinteger(L, 1);
    const auto child = std::ranges::find(spawned_, id, &Spawned::id);
    if (child != spawned_.end()) {
        drain(*child);
    }
    const bool asked = child != spawned_.end() && !child->ended && kill(child->pid, SIGTERM) == 0;
    lua_pushboolean(L, asked ? 1 : 0);
    return 1;
}

int SpawnPool::wait(lua_State* L, double deadline) {
    const bool limited = !lua_isnoneornil(L, 1);
    double until = limited ? now() + within_limit(deadline, luaL_checknumber(L, 1)) : 0;
    if (deadline > 0) {
        until = until > 0 ? std::min(until, deadline - kDeadlineSlack) : deadline - kDeadlineSlack;
    }
    for (;;) {
        raise_if_signaled(L);
        for (Spawned& child : spawned_) {
            drain(child);
        }
        if (const auto done = std::ranges::find(spawned_, true, &Spawned::ended); done != spawned_.end()) {
            const Spawned child = std::move(*done);
            spawned_.erase(done);
            reap_exited();
            bool returned = false;
            std::string_view text = "the child ended without a result: it was killed, or ran out of memory";
            static_cast<void>(unframe(child.data, returned, text));
            lua_pushinteger(L, child.id);
            lua_pushboolean(L, returned ? 1 : 0);
            lua_pushlstring(L, text.data(), text.size());
            return 3;
        }
        if (spawned_.empty() || (until > 0 && now() >= until)) {
            lua_pushnil(L);
            lua_pushinteger(L, static_cast<int>(spawned_.size()));
            return 2;
        }
        std::vector<pollfd> waiters;
        waiters.reserve(spawned_.size());
        for (const Spawned& child : spawned_) {
            waiters.push_back({.fd = child.result.get(), .events = POLLIN, .revents = 0});
        }
        wait_for(waiters, until > 0 ? std::max(until - now(), 0.0) : -1.0);
    }
}

}  // namespace hx
