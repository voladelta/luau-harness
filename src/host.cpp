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

bool continues(char c) {
    return (static_cast<unsigned char>(c) & 0xC0U) == 0x80U;
}

}  // namespace

std::string_view cut_head(std::string_view head) {
    if (const std::size_t line = head.rfind('\n'); line != std::string_view::npos && 2 * (line + 1) > head.size()) {
        return head.substr(0, line + 1);
    }
    while (!head.empty() && continues(head.back())) {
        head.remove_suffix(1);
    }
    if (!head.empty() && static_cast<unsigned char>(head.back()) >= 0xC0U) {
        head.remove_suffix(1);
    }
    return head;
}

std::string_view cut_tail(std::string_view tail) {
    if (const std::size_t line = tail.find('\n');
        line != std::string_view::npos && 2 * (tail.size() - line - 1) > tail.size()) {
        return tail.substr(line + 1);
    }
    while (!tail.empty() && continues(tail.front())) {
        tail.remove_prefix(1);
    }
    return tail;
}

namespace {

class TryScope {
  public:
    explicit TryScope(int& depth) noexcept : depth_(depth) { ++depth_; }
    TryScope(const TryScope&) = delete;
    TryScope& operator=(const TryScope&) = delete;
    ~TryScope() { --depth_; }

  private:
    int& depth_;
};

struct LimitTimer {
    timer_t timer{};
    bool made = false;
};

LimitTimer make_limit_timer() noexcept {
    LimitTimer timer;
    sigevent event{};
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGALRM;
    timer.made = timer_create(CLOCK_MONOTONIC, &event, &timer.timer) == 0;
    return timer;
}

LimitTimer& the_limit_timer() noexcept {
    static LimitTimer timer = make_limit_timer();
    return timer;
}

timer_t* limit_timer() noexcept {
    LimitTimer& timer = the_limit_timer();
    return timer.made ? &timer.timer : nullptr;
}

void arm_limit(double deadline) noexcept {
    const timer_t* const timer = limit_timer();
    if (timer == nullptr) {
        return;
    }
    const auto whole = static_cast<time_t>(deadline);
    const itimerspec at{
        .it_interval = {.tv_sec = 0, .tv_nsec = 0},
        .it_value = {.tv_sec = whole, .tv_nsec = static_cast<long>((deadline - static_cast<double>(whole)) * 1e9)}};
    timer_settime(*timer, TIMER_ABSTIME, &at, nullptr);
}

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
            std::cin.clear();
            std::clearerr(stdin);
            raise_if_signaled(L);
        }
        return 0;
    }
    lua_pushlstring(L, line.data(), line.size());
    return 1;
}

int l_clip(lua_State* L) {
    std::size_t length = 0;
    const char* raw = luaL_checklstring(L, 1, &length);
    const double limit = luaL_checknumber(L, 2);
    if (static_cast<double>(length) <= limit) {
        lua_settop(L, 1);
        return 1;
    }
    const std::size_t half = limit > 0 ? static_cast<std::size_t>(limit / 2) : 0;
    const std::string_view text(raw, length);
    const std::string_view head = cut_head(text.substr(0, half));
    const std::string_view tail = cut_tail(text.substr(length - half));
    lua_pushlstring(L, head.data(), head.size());
    lua_pushlstring(L, tail.data(), tail.size());
    return 2;
}

std::size_t unprintable_tail(std::string_view kept) {
    const std::size_t n = kept.size();
    const auto at = [&](std::size_t back) { return static_cast<unsigned char>(kept[n - back]); };
    if (n >= 2 && at(2) == 0xC2 && (at(1) & 0xE0U) == 0x80U) {
        return 2;
    }
    if (n >= 3 && at(3) == 0xE2 &&
        ((at(2) == 0x80 && at(1) >= 0xAA && at(1) <= 0xAE) || (at(2) == 0x81 && at(1) >= 0xA6 && at(1) <= 0xA9))) {
        return 3;
    }
    return 0;
}

int l_printable(lua_State* L) {
    std::size_t length = 0;
    const char* raw = luaL_checklstring(L, 1, &length);
    std::string kept;
    kept.reserve(length);
    for (const char each : std::string_view(raw, length)) {
        const auto c = static_cast<unsigned char>(each);
        if (c == '\t' || c == '\n' || (c >= 0x20 && c != 0x7F)) {
            kept.push_back(each);
            kept.resize(kept.size() - unprintable_tail(kept));
        }
    }
    lua_pushlstring(L, kept.data(), kept.size());
    return 1;
}

int l_log(lua_State* L) {
    std::size_t length = 0;
    const char* text = luaL_checklstring(L, 1, &length);
    fmt::print(stderr, "{}\n", std::string_view(text, length));
    return 0;
}

int l_env(lua_State* L) {
    const char* value = std::getenv(check_cstring(L, 1));  // NOLINT(concurrency-mt-unsafe)
    if (value == nullptr) {
        return 0;
    }
    lua_pushstring(L, value);
    return 1;
}

}  // namespace

void renew_limit_timer() noexcept {
    the_limit_timer() = make_limit_timer();
    g_expired = 1;
}

Host::Host(HostConfig config, const Sandbox& sandbox) : config_(std::move(config)), sandboxed_(sandbox.confined()) {
    prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
}

Host::~Host() {
    kill_descendants();
}

Host& Host::of(lua_State* L) {
    return *static_cast<Host*>(lua_callbacks(L)->userdata);
}

namespace {

bool g_memory_refused = false;  // NOLINT(cppcoreguidelines-avoid-non-const-global-variables)

}  // namespace

void vm_memory_refused() noexcept {
    g_memory_refused = true;
}

void Host::on_interrupt(lua_State* L, int gc) {
    if (gc >= 0) {
        return;
    }
    if (g_memory_refused) {
        g_memory_refused = false;
        lua_gc(L, LUA_GCCOLLECT, 0);
    }
    raise_if_signaled(L);
    if (g_expired == 0) {
        return;
    }
    g_expired = limit_timer() != nullptr ? 0 : 1;
    const Host& host = of(L);
    if (host.deadline_ > 0 && now() >= host.deadline_) {
        g_expired = 1;
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
        wait_for({}, left);
    }
    raise_if_signaled(L);
    return 0;
}

int Host::protect(lua_State* L, int fn, double deadline) {
    luaL_checktype(L, fn, LUA_TFUNCTION);
    Host& host = of(L);
    int status = 0;
    {
        const DeadlineScope limit(host.deadline_, deadline);
        const TryScope scope(host.tries_);
        if (g_signal == 0) {
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

int Host::try_(lua_State* L) {
    return protect(L, 1, 0);
}

int Host::timed(lua_State* L) {
    const double seconds = duration(luaL_checknumber(L, 1));
    return protect(L, 2, now() + seconds);
}

void Host::open(lua_State* L) {
    lua_Callbacks* callbacks = lua_callbacks(L);
    callbacks->userdata = this;
    callbacks->interrupt = on_interrupt;

    static constexpr luaL_Reg kFunctions[] = {
        {"post", post},      {"route", route},      {"sh", sh},       {"try", try_},    {"timed", timed},
        {"read", read_file}, {"write", write_file}, {"list", l_list}, {"load", l_load}, {"input", l_input},
        {"log", l_log},      {"sleep", sleep},      {"env", l_env},   {"judge", judge}, {"clip", l_clip},
        {"spawn", spawn},    {"wait", wait},        {"stat", l_stat}, {"walk", l_walk}, {"printable", l_printable},
        {nullptr, nullptr},
    };
    luaL_register(L, "host", kFunctions);

    push_strings(L, config_.args);
    lua_setreadonly(L, -1, 1);
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
    lua_createtable(L, 0, static_cast<int>(config_.endpoints.size()));
    for (const Endpoint& each : config_.endpoints) {
        lua_pushboolean(L, each.key_env.empty() || !each.key.empty() ? 1 : 0);
        lua_setfield(L, -2, each.name.c_str());
    }
    lua_setreadonly(L, -1, 1);
    lua_setfield(L, -2, "endpoints");
    lua_pushboolean(L, config_.judge.valid() ? 1 : 0);
    lua_setfield(L, -2, "judged");
    if (!config_.workflow.empty()) {
        lua_pushlstring(L, config_.workflow.data(), config_.workflow.size());
        lua_setfield(L, -2, "workflow");
    }
    for (const auto& [field, texts] :
         {std::pair{"extensions", &config_.extensions}, std::pair{"skills", &config_.skills}}) {
        lua_createtable(L, 0, static_cast<int>(texts->size()));
        for (const auto& [name, text] : *texts) {
            lua_pushlstring(L, text.data(), text.size());
            lua_setfield(L, -2, name.c_str());
        }
        lua_setreadonly(L, -1, 1);
        lua_setfield(L, -2, field);
    }
    lua_pop(L, 1);
}

}  // namespace hx
