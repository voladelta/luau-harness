#include <poll.h>
#include <sys/socket.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <cstddef>
#include <cstring>
#include <string>

#include "host.h"
#include "host_internal.h"
#include "lua.h"
#include "lualib.h"
#include "sys.h"

namespace hx {

int JudgeChannel::exchange(lua_State* L, double deadline) {
    constexpr std::size_t kMaxLine = std::size_t{64} << 10;
    std::size_t length = 0;
    const char* line = luaL_optlstring(L, 1, nullptr, &length);
    if (!fd_.valid()) {
        luaL_error(L, "host.judge: no judge in this session (start hx with --judge KIT)");
    }
    if (line != nullptr && (std::memchr(line, '\n', length) != nullptr || length > kMaxLine)) {
        luaL_error(L, "host.judge: send one line, without a newline, of at most 64 KiB");
    }
    raise_if_signaled(L);
    const int fd = fd_.get();
    if (line != nullptr && owed_ > 0) {
        luaL_error(L, "host.judge: the judge is still judging the last submission; wait for its verdict first");
    }
    if (line != nullptr) {
        if (!write_all(fd, std::string(line, length) + "\n")) {
            luaL_error(L, "host.judge: the judge is gone");
        }
        ++owed_;
    }
    const double until = deadline > 0 ? deadline - kDeadlineSlack : 0;
    std::array<char, kBufferSize> buffer{};
    lua_newtable(L);
    int answers = 0;
    for (;;) {
        for (std::size_t end = in_.find('\n'); end != std::string::npos; end = in_.find('\n')) {
            lua_pushlstring(L, in_.data(), end);
            lua_rawseti(L, -2, ++answers);
            in_.erase(0, end + 1);
            --owed_;
        }
        raise_if_signaled(L);
        if (owed_ == 0 || (until > 0 && now() >= until)) {
            lua_pushinteger(L, owed_);
            return 2;
        }
        pollfd waiter{.fd = fd, .events = POLLIN, .revents = 0};
        wait_for({&waiter, 1}, until > 0 ? std::max(until - now(), 0.0) : -1.0);
        const ssize_t n = recv(fd, buffer.data(), buffer.size(), MSG_DONTWAIT);
        if (n == 0 || (n < 0 && errno != EAGAIN && errno != EINTR)) {
            luaL_error(L, "host.judge: the judge is gone");
        }
        if (n > 0) {
            in_.append(buffer.data(), static_cast<std::size_t>(n));
        }
    }
}

}  // namespace hx
