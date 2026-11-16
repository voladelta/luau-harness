#ifndef HX_HOST_INTERNAL_H_
#define HX_HOST_INTERNAL_H_

// What the parts of Host share: the stop and time-limit flags, the clock, how a primitive returns or raises an
// error, and the waits and kills a stop reaches. For the host*.cpp files only; the rest of hx uses host.h.

#include <poll.h>
#include <sys/types.h>

#include <csignal>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include "lua.h"

// The host functions raise Luau errors with C++ objects alive on the stack, which only an error thrown as a C++
// exception destroys; a longjmp would skip their destructors.
#if LUA_USE_LONGJMP
#error "hx needs Luau built to raise errors as C++ exceptions, not with longjmp"
#endif

namespace hx {

inline constexpr double kReapSeconds = 1.0;  // how long hx waits for a process it killed to be gone
inline constexpr double kLongest = 1e9;      // seconds, about 30 years: the longest duration a call can ask for

// Signal handlers cannot be given context, so this state is global. It is the only mutable global state in hx;
// host_proc.cpp defines it.
// NOLINTBEGIN(cppcoreguidelines-avoid-non-const-global-variables)
extern volatile std::sig_atomic_t g_signal;   // a stop in progress: the signal that asked for it
extern volatile std::sig_atomic_t g_expired;  // the time limit's timer has fired: the next safepoint reads the clock
// NOLINTEND(cppcoreguidelines-avoid-non-const-global-variables)

// --- host.cpp: between Luau and the host ---------------------------------------------------------------

// The conventional "nil, message" return for a failure the caller is expected to handle.
int fail(lua_State* L, std::string_view message);
// "nil, PATH: what errno says".
int fail_errno(lua_State* L, std::string_view path);
// A string argument that goes to the system as a C string. One with a NUL byte in it would be cut short there
// without a word, so it is refused.
const char* check_cstring(lua_State* L, int idx);
// Reads an optional numeric field of the options table at idx.
double option(lua_State* L, int idx, const char* name, double fallback);
// A duration from Luau in seconds. A negative one (or nan) is empty, and a huge one is cut to kLongest, so every
// conversion to milliseconds or nanoseconds stays defined.
double duration(double seconds);
// The Luau error a stop raises: "interrupted" for SIGINT, "terminated" for the others.
const char* stop_error(int sig);
// Raises error as a Luau error.
[[noreturn]] void raise_error(lua_State* L, const char* error);
// Raises the stop in progress as its Luau error. The stop is not over: every later safepoint and blocking call
// raises it again, until the outermost host.try or host.timed returns, so no pcall can swallow it.
void raise_if_signaled(lua_State* L);
// Pushes a Luau list of the strings.
void push_strings(lua_State* L, const std::vector<std::string>& strings);
// Makes the time limit's timer again. A forked child has none of its parent's timers, and without one of its own
// no host.timed limit in it would ever fire.
void renew_limit_timer() noexcept;

// What host.sh and host.clip keep of a long text: its start and its end, each cut where a reader loses least. A part
// ends or starts at a line when that keeps more than half of it; otherwise it is cut at a character, never inside
// one, since half a character is not valid UTF-8 and would be lost without a word.
// cut_head: the start of a text that goes on past it. cut_tail: the end of a text that began before it.
std::string_view cut_head(std::string_view head);
std::string_view cut_tail(std::string_view tail);

// --- host_files.cpp ------------------------------------------------------------------------------------------

// host.read and host.write are members of Host, which holds the time limit they wait within.
int l_list(lua_State* L);

// --- host_proc.cpp: signals, waits and the processes hx started ----------------------------------------

// The monotonic clock, in seconds.
double now() noexcept;
// Waits until one of fds is ready, for at most seconds (negative: without limit), or until a signal arrives.
void wait_for(std::span<pollfd> fds, double seconds) noexcept;
// Kills every descendant of hx that is not spared (a sorted list of PIDs), then waits a moment for them to be gone.
void kill_descendants(std::span<const pid_t> spared = {}) noexcept;
// The descendants of hx now, sorted: what kill_descendants spares after a command behind a narrower wall.
std::vector<pid_t> descendants_now();
// Reaps every child that has exited.
void reap_exited() noexcept;

}  // namespace hx

#endif  // HX_HOST_INTERNAL_H_
