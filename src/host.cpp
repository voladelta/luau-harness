// The Host and the VM: the `host` table, stops and time limits at safepoints, host.try and host.timed, and the
// small primitives. The rest of the primitives are in the host_*.cpp files.

#include "host.h"

#include <sys/prctl.h>
#include <unistd.h>

#include <algorithm>
#include <csignal>
#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <filesystem>
#include <iostream>
#include <memory>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <fmt/format.h>

#include "host_internal.h"
#include "lua.h"
#include "luacode.h"
#include "lualib.h"
#include "sys.h"

namespace hx {

// --- between Luau and the host --------------------------------------------------------------------------------

int fail(lua_State* L, std::string_view message) {
    lua_pushnil(L);
    lua_pushlstring(L, message.data(), message.size());
    return 2;
}

int fail_errno(lua_State* L, std::string_view path) {
    return fail(L, fmt::format("{}: {}", path, error_text()));
}

const char* check_cstring(lua_State* L, int idx) {
    std::size_t length = 0;
    const char* text = luaL_checklstring(L, idx, &length);
    if (std::strlen(text) != length) {
        luaL_argerror(L, idx, "contains a NUL byte");
    }
    return text;
}

double option(lua_State* L, int idx, const char* name, double fallback) {
    if (lua_isnoneornil(L, idx)) {
        return fallback;
    }
    luaL_checktype(L, idx, LUA_TTABLE);
    lua_getfield(L, idx, name);
    const double value = lua_isnil(L, -1) ? fallback : luaL_checknumber(L, -1);
    lua_pop(L, 1);
    return value;
}

double duration(double seconds) {
    return seconds > 0 ? std::min(seconds, kLongest) : 0.0;
}

const char* stop_error(int sig) {
    return sig == SIGINT ? "interrupted" : "terminated";
}

void raise_error(lua_State* L, const char* error) {
    lua_pushstring(L, error);
    lua_error(L);
}

void raise_if_signaled(lua_State* L) {
    if (g_signal != 0) {
        raise_error(L, stop_error(g_signal));
    }
}

void push_strings(lua_State* L, const std::vector<std::string>& strings) {
    lua_createtable(L, static_cast<int>(strings.size()), 0);
    int index = 0;
    for (const std::string& each : strings) {
        lua_pushlstring(L, each.data(), each.size());
        lua_rawseti(L, -2, ++index);
    }
}

namespace {

// --- time limits ----------------------------------------------------------------------------------------------

// Counts a host.try or host.timed call for as long as it runs.
class TryScope {
  public:
    explicit TryScope(int& depth) noexcept : depth_(depth) { ++depth_; }
    TryScope(const TryScope&) = delete;
    TryScope& operator=(const TryScope&) = delete;
    ~TryScope() { --depth_; }

  private:
    int& depth_;
};

// The process's one timer, which raises SIGALRM at the time limit in force, or null when the kernel gave none.
timer_t* limit_timer() noexcept {
    static timer_t timer{};
    static const bool made = [] {
        sigevent event{};
        event.sigev_notify = SIGEV_SIGNAL;
        event.sigev_signo = SIGALRM;
        return timer_create(CLOCK_MONOTONIC, &event, &timer) == 0;
    }();
    return made ? &timer : nullptr;
}

// Sets the timer to fire at deadline, a time on the monotonic clock; 0 turns it off. A deadline already past fires
// at once. The VM is interrupted by a flag the signal sets, so a tight loop pays for no clock read, and the limit is
// as exact as the next safepoint, however slow the steps between safepoints are.
void arm_limit(double deadline) noexcept {
    timer_t* timer = limit_timer();
    if (timer == nullptr) {
        return;  // g_expired stays set, and every safepoint reads the clock
    }
    const auto whole = static_cast<time_t>(deadline);
    const itimerspec at{{0, 0}, {whole, static_cast<long>((deadline - static_cast<double>(whole)) * 1e9)}};
    timer_settime(*timer, TIMER_ABSTIME, &at, nullptr);
}

// Tightens a time limit for as long as the scope lives, then puts the outer limit back. A limit of 0 means none.
class DeadlineScope {
  public:
    DeadlineScope(double& slot, double limit) noexcept : slot_(slot), outer_(slot) {
        slot_ = limit <= 0 || (outer_ > 0 && outer_ < limit) ? outer_ : limit;
        arm_limit(slot_);
    }
    DeadlineScope(const DeadlineScope&) = delete;
    DeadlineScope& operator=(const DeadlineScope&) = delete;
    ~DeadlineScope() {
        slot_ = outer_;
        arm_limit(slot_);
    }

  private:
    double& slot_;
    double outer_;
};

// --- small primitives -----------------------------------------------------------------------------------------

int l_load(lua_State* L) {
    std::size_t length = 0;
    const char* source = luaL_checklstring(L, 1, &length);
    const char* name = luaL_optstring(L, 2, "=chunk");
    const bool has_env = !lua_isnoneornil(L, 3);
    if (has_env) {
        luaL_checktype(L, 3, LUA_TTABLE);
    }

    std::size_t bytecode_size = 0;
    const std::unique_ptr<char, FreeDeleter> bytecode(luau_compile(source, length, nullptr, &bytecode_size));
    if (!bytecode) {
        luaL_error(L, "out of memory compiling %s", name);
    }
    // A syntax error travels inside the bytecode and comes out of luau_load as its message.
    if (luau_load(L, name, bytecode.get(), bytecode_size, 0) != 0) {
        lua_pushnil(L);
        lua_insert(L, -2);
        return 2;
    }
    if (has_env) {
        lua_pushvalue(L, 3);
        lua_setfenv(L, -2);
    }
    return 1;
}

int l_input(lua_State* L) {
    raise_if_signaled(L);
    const char* prompt = luaL_optstring(L, 1, "");
    fmt::print(stderr, "{}", prompt);
    static_cast<void>(std::fflush(stderr));
    std::string line;
    if (!std::getline(std::cin, line)) {
        if (g_signal != 0) {
            // The read was cut short, not ended. Left as it is, the stream would report the end of input to
            // every later call, and a loop that outlives the stop could never read again.
            std::cin.clear();
            std::clearerr(stdin);
            raise_if_signaled(L);
        }
        return 0;
    }
    lua_pushlstring(L, line.data(), line.size());
    return 1;
}

int l_log(lua_State* L) {
    std::size_t length = 0;
    const char* text = luaL_checklstring(L, 1, &length);
    fmt::print(stderr, "{}\n", std::string_view(text, length));
    return 0;
}

int l_env(lua_State* L) {
    const char* value = std::getenv(check_cstring(L, 1));  // NOLINT(concurrency-mt-unsafe): single-threaded
    if (value == nullptr) {
        return 0;
    }
    lua_pushstring(L, value);
    return 1;
}

}  // namespace

// --- the Host -------------------------------------------------------------------------------------------------

Host::Host(HostConfig config, const Sandbox& sandbox) : config_(std::move(config)), sandboxed_(sandbox.confined()) {
    // An orphan comes to hx instead of init, whatever session it moved to, so every process a command started stays
    // a descendant of hx and ~Host can find it.
    prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
}

Host::~Host() {
    kill_descendants();
}

Host& Host::of(lua_State* L) {
    return *static_cast<Host*>(lua_callbacks(L)->userdata);
}

// Runs at VM safepoints (loop back edges, calls). Luau code cannot block these, so a signal or an expired time
// limit always reaches the code that is running. Both are flags a signal handler sets, so a safepoint costs two
// loads; the clock is read only once the limit's timer has fired (see arm_limit).
void Host::on_interrupt(lua_State* L, int gc) {
    if (gc >= 0) {
        return;  // inside the collector: raising here is not allowed
    }
    raise_if_signaled(L);
    if (g_expired == 0) {
        return;
    }
    // Cleared before the clock is read, so a timer that fires meanwhile is not lost. It may have fired for a limit
    // that has ended since; then the clock says so and nothing is raised.
    g_expired = limit_timer() != nullptr ? 0 : 1;
    const Host& host = of(L);
    if (host.deadline_ > 0 && now() >= host.deadline_) {
        g_expired = 1;  // raised again at every safepoint until the limit's call returns, so no pcall swallows it
        raise_error(L, "timeout");
    }
}

double Host::within_limit(double seconds) const {
    seconds = duration(seconds);
    if (deadline_ <= 0) {
        return seconds;
    }
    return std::clamp(deadline_ - now(), 0.0, seconds);
}

int Host::sleep(lua_State* L) {
    raise_if_signaled(L);
    const double until = now() + of(L).within_limit(luaL_checknumber(L, 1));
    while (g_signal == 0) {
        const double left = until - now();
        if (left <= 0) {
            break;
        }
        wait_for({}, left);  // a signal ends it at once
    }
    raise_if_signaled(L);
    return 0;
}

// Calls the function at stack index fn with the arguments above it, under a time limit (0 for none), and returns
// true and its results or false and its error. This is where a stop ends: a call that returns while a stop is in
// progress returns false and the stop's error, whatever the function did, and the outermost such call also ends
// the stop.
int Host::protect(lua_State* L, int fn, double deadline) {
    luaL_checktype(L, fn, LUA_TFUNCTION);
    Host& host = of(L);
    int status = 0;
    {
        const DeadlineScope limit(host.deadline_, deadline);
        const TryScope scope(host.tries_);
        if (g_signal == 0) {  // a stop in progress does not start the function
            status = lua_pcall(L, lua_gettop(L) - fn, LUA_MULTRET, 0);
        }
    }
    if (const int sig = g_signal; sig != 0) {
        if (host.tries_ == 0) {
            g_signal = 0;
        }
        lua_settop(L, 0);
        lua_pushboolean(L, 0);
        lua_pushstring(L, stop_error(sig));
        return 2;
    }
    lua_pushboolean(L, status == 0 ? 1 : 0);
    lua_insert(L, fn);
    return lua_gettop(L) - fn + 1;
}

// host.try(fn, ...): a pcall that is also where a stop ends.
int Host::try_(lua_State* L) {
    return protect(L, 1, 0);
}

// host.timed(seconds, fn, ...): host.try whose callee is stopped with the error "timeout" once the limit passes.
// Limits nest: an inner call cannot outlast the outer one. llm, sh and sleep give up when the limit passes, and
// the error is raised as soon as Luau code runs again.
int Host::timed(lua_State* L) {
    const double seconds = duration(luaL_checknumber(L, 1));
    return protect(L, 2, now() + seconds);
}

void Host::open(lua_State* L) {
    lua_Callbacks* callbacks = lua_callbacks(L);
    callbacks->userdata = this;
    callbacks->interrupt = on_interrupt;

    static constexpr luaL_Reg kFunctions[] = {
        {"llm", llm},     {"tool", tool},     {"sh", sh},       {"try", try_},    {"timed", timed},
        {"read", l_read}, {"write", l_write}, {"list", l_list}, {"load", l_load}, {"input", l_input},
        {"log", l_log},   {"sleep", sleep},   {"env", l_env},   {"judge", judge}, {nullptr, nullptr},
    };
    luaL_register(L, "host", kFunctions);

    push_strings(L, config_.args);
    lua_setfield(L, -2, "args");

    std::error_code ec;
    const std::string cwd = std::filesystem::current_path(ec).string();
    lua_pushlstring(L, cwd.data(), cwd.size());
    lua_setfield(L, -2, "cwd");
    lua_pushboolean(L, config_.interactive ? 1 : 0);
    lua_setfield(L, -2, "interactive");
    lua_pushboolean(L, config_.quiet ? 1 : 0);
    lua_setfield(L, -2, "quiet");
    lua_pushboolean(L, sandboxed_ ? 1 : 0);
    lua_setfield(L, -2, "sandboxed");
    lua_pushlstring(L, config_.default_loop.data(), config_.default_loop.size());
    lua_setfield(L, -2, "default_loop");
    lua_pushboolean(L, isatty(STDOUT_FILENO) != 0 ? 1 : 0);
    lua_setfield(L, -2, "tty");
    lua_pushlstring(L, config_.model.data(), config_.model.size());
    lua_setfield(L, -2, "model");
    // host.tools speaks for DeepSeek's endpoints, which share one key, and host.exa for Exa's.
    const auto has_key = [this](std::string_view name) {
        const Endpoint* found = endpoint(name);
        return found != nullptr && !found->key.empty();
    };
    lua_pushboolean(L, has_key("search") ? 1 : 0);
    lua_setfield(L, -2, "tools");
    lua_pushboolean(L, has_key("exa") ? 1 : 0);
    lua_setfield(L, -2, "exa");
    lua_pushboolean(L, config_.judge.valid() ? 1 : 0);
    lua_setfield(L, -2, "judged");
    lua_createtable(L, 0, static_cast<int>(config_.extensions.size()));
    for (const auto& [name, source] : config_.extensions) {
        lua_pushlstring(L, source.data(), source.size());
        lua_setfield(L, -2, name.c_str());
    }
    lua_setfield(L, -2, "extensions");
    lua_pop(L, 1);
}

}  // namespace hx
