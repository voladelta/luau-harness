#ifndef HX_HOST_INTERNAL_H_
#define HX_HOST_INTERNAL_H_

#include <poll.h>
#include <sys/types.h>

#include <csignal>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include "lua.h"

#if LUA_USE_LONGJMP
#error "hx needs Luau built to raise errors as C++ exceptions, not with longjmp"
#endif

namespace hx {

inline constexpr double kReapSeconds = 1.0;
inline constexpr double kLongest = 1e9;

// NOLINTBEGIN(cppcoreguidelines-avoid-non-const-global-variables)
extern volatile std::sig_atomic_t g_signal;
extern volatile std::sig_atomic_t g_expired;
// NOLINTEND(cppcoreguidelines-avoid-non-const-global-variables)

int fail(lua_State* L, std::string_view message);
int fail_errno(lua_State* L, std::string_view path);
const char* check_cstring(lua_State* L, int idx);
double option(lua_State* L, int idx, const char* name, double fallback);
double duration(double seconds);
const char* stop_error(int sig);
[[noreturn]] void raise_error(lua_State* L, const char* error);
void raise_if_signaled(lua_State* L);
void push_strings(lua_State* L, const std::vector<std::string>& strings);
void renew_limit_timer() noexcept;

std::string_view cut_head(std::string_view head);
std::string_view cut_tail(std::string_view tail);

int l_list(lua_State* L);

double now() noexcept;
void wait_for(std::span<pollfd> fds, double seconds) noexcept;
void kill_descendants(std::span<const pid_t> spared = {}) noexcept;
std::vector<pid_t> descendants_now();
void reap_exited() noexcept;

}  // namespace hx

#endif  // HX_HOST_INTERNAL_H_
