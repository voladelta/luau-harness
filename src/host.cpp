#include "host.h"

#include <fcntl.h>
#include <sys/prctl.h>
#include <unistd.h>

#include <algorithm>
#include <array>
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

double within_limit(double deadline, double seconds) {
    seconds = duration(seconds);
    if (deadline <= 0) {
        return seconds;
    }
    return std::clamp(deadline - now(), 0.0, seconds);
}

const char* stop_error(int sig) {
    return sig == SIGINT ? "interrupted" : "terminated";
}

int stop_signal(std::string_view error) noexcept {
    for (const int sig : {SIGINT, SIGTERM}) {
        if (error == stop_error(sig)) {
            return sig;
        }
    }
    return 0;
}

void raise_error(lua_State* L, const char* error) {
    lua_pushstring(L, error);
    lua_error(L);
}

void raise_if_signaled(lua_State* L) {
    if (const int sig = pending_signal(); sig != 0) {
        raise_error(L, stop_error(sig));
    }
}

std::pair<UniqueFd, UniqueFd> make_pipe(lua_State* L) {
    std::array<int, 2> ends{};
    if (pipe2(ends.data(), O_CLOEXEC) != 0) {
        luaL_error(L, "pipe: %s", error_text().c_str());
    }
    return {UniqueFd(ends[0]), UniqueFd(ends[1])};
}

void push_strings(lua_State* L, const std::vector<std::string>& strings) {
    lua_createtable(L, static_cast<int>(strings.size()), 0);
    int index = 0;
    for (const std::string& each : strings) {
        lua_pushlstring(L, each.data(), each.size());
        lua_rawseti(L, -2, ++index);
    }
}

void set_text(lua_State* L, const char* field, std::string_view text) {
    lua_pushlstring(L, text.data(), text.size());
    lua_setfield(L, -2, field);
}

void set_number(lua_State* L, const char* field, double number) {
    lua_pushnumber(L, number);
    lua_setfield(L, -2, field);
}

void set_flag(lua_State* L, const char* field, bool flag) {
    lua_pushboolean(L, flag ? 1 : 0);
    lua_setfield(L, -2, field);
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
        if (pending_signal() != 0) {
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

std::size_t mark_at_end(std::string_view text) {
    const auto back = [&](std::size_t i) { return static_cast<unsigned char>(text[text.size() - i]); };
    if (text.size() >= 3 && back(3) == 0xE2 &&
        ((back(2) == 0x80 && back(1) >= 0xAA && back(1) <= 0xAE) ||
         (back(2) == 0x81 && back(1) >= 0xA6 && back(1) <= 0xA9))) {
        return 3;
    }
    if (text.size() >= 2 && back(2) == 0xC2 && (back(1) & 0xE0U) == 0x80U) {
        return 2;
    }
    return 0;
}

int l_printable(lua_State* L) {
    std::size_t length = 0;
    const char* raw = luaL_checklstring(L, 1, &length);
    std::string kept;
    kept.reserve(length);
    for (const char byte : std::string_view(raw, length)) {
        const auto c = static_cast<unsigned char>(byte);
        if (c == '\t' || c == '\n' || (c >= 0x20 && c != 0x7F)) {
            kept.push_back(byte);
            kept.resize(kept.size() - mark_at_end(kept));
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

int l_wall_time(lua_State* L) {
    timespec stamp{};
    if (clock_gettime(CLOCK_REALTIME, &stamp) != 0) {
        luaL_error(L, "clock_gettime: %s", error_text().c_str());
    }
    lua_pushnumber(L, static_cast<double>(stamp.tv_sec));
    lua_pushnumber(L, static_cast<double>(stamp.tv_nsec));
    return 2;
}

}  // namespace

Host::Host(HostConfig config, const Sandbox& sandbox, bool* memory_refused)
    : config_(std::move(config)),
      sandboxed_(sandbox.confined()),
      memory_refused_(memory_refused),
      http_(std::make_unique<HttpClient>(std::move(config_.endpoints), config_.model, std::move(config_.ca_bundle))),
      judge_(std::make_unique<JudgeChannel>(std::move(config_.judge))),
      spawn_(std::make_unique<SpawnPool>()) {
    prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
}

Host::~Host() {
    kill_descendants();
}

Host& Host::of(lua_State* L) {
    return *static_cast<Host*>(lua_callbacks(L)->userdata);
}

void Host::on_interrupt(lua_State* L, int gc) {
    if (gc >= 0) {
        return;
    }
    const Host& host = of(L);
    if (host.memory_refused_ != nullptr && *host.memory_refused_) {
        *host.memory_refused_ = false;
        lua_gc(L, LUA_GCCOLLECT, 0);
    }
    raise_if_signaled(L);
    if (!take_limit_check()) {
        return;
    }
    if (host.deadline_ > 0 && now() >= host.deadline_) {
        keep_limit_check();
        raise_error(L, "timeout");
    }
}

void Host::check_deadline(lua_State* L) {
    on_interrupt(L, -1);
}

int Host::post(lua_State* L) {
    Host& host = of(L);
    return host.http_->post(L, host.deadline_);
}

int Host::route(lua_State* L) {
    return of(L).http_->route(L);
}

int Host::sh(lua_State* L) {
    const Host& host = of(L);
    return run_sh(L, host.deadline_, host.config_.system_rules);
}

int Host::read_file(lua_State* L) {
    return read_file_lua(L, of(L).deadline_);
}

int Host::write_file(lua_State* L) {
    return write_file_lua(L, of(L).deadline_);
}

int Host::judge(lua_State* L) {
    Host& host = of(L);
    return host.judge_->exchange(L, host.deadline_);
}

int Host::spawn(lua_State* L) {
    Host& host = of(L);
    return host.spawn_->spawn(
        L, [&] { host.http_->reset_handle(); },
        [&] {
            host.judge_->close();
            host.deadline_ = 0;
            ++host.tries_;
        });
}

int Host::wait(lua_State* L) {
    Host& host = of(L);
    return host.spawn_->wait(L, host.deadline_);
}

int Host::stop(lua_State* L) {
    return of(L).spawn_->stop(L);
}

int Host::sleep(lua_State* L) {
    raise_if_signaled(L);
    const double deadline = of(L).deadline_;
    const double requested = now() + duration(luaL_checknumber(L, 1));
    const double until = deadline > 0 ? std::min(deadline, requested) : requested;
    while (pending_signal() == 0) {
        const double left = until - now();
        if (left <= 0) {
            break;
        }
        wait_for({}, left);
    }
    raise_if_signaled(L);
    if (deadline > 0 && now() >= deadline) {
        keep_limit_check();
        raise_error(L, "timeout");
    }
    return 0;
}

int Host::protect(lua_State* L, int fn, double deadline) {
    luaL_checktype(L, fn, LUA_TFUNCTION);
    Host& host = of(L);
    int status = 0;
    {
        const DeadlineScope limit(host.deadline_, deadline);
        const TryScope scope(host.tries_);
        if (pending_signal() == 0) {
            if (host.deadline_ > 0 && now() >= host.deadline_) {
                keep_limit_check();
                lua_settop(L, fn - 1);
                lua_pushliteral(L, "timeout");
                status = LUA_ERRRUN;
            } else {
                status = lua_pcall(L, lua_gettop(L) - fn, LUA_MULTRET, 0);
            }
        }
    }
    if (const int sig = pending_signal(); sig != 0) {
        if (host.tries_ == 0) {
            forget_signal();
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
        {.name = "post", .func = post},
        {.name = "route", .func = route},
        {.name = "sh", .func = sh},
        {.name = "try", .func = try_},
        {.name = "timed", .func = timed},
        {.name = "read", .func = read_file},
        {.name = "write", .func = write_file},
        {.name = "list", .func = l_list},
        {.name = "load", .func = l_load},
        {.name = "input", .func = l_input},
        {.name = "log", .func = l_log},
        {.name = "sleep", .func = sleep},
        {.name = "env", .func = l_env},
        {.name = "wall_time", .func = l_wall_time},
        {.name = "judge", .func = judge},
        {.name = "clip", .func = l_clip},
        {.name = "spawn", .func = spawn},
        {.name = "wait", .func = wait},
        {.name = "stop", .func = stop},
        {.name = "stat", .func = l_stat},
        {.name = "walk", .func = l_walk},
        {.name = "printable", .func = l_printable},
        {.name = nullptr, .func = nullptr},
    };
    luaL_register(L, "host", kFunctions);

    push_strings(L, config_.args);
    lua_setreadonly(L, -1, 1);
    lua_setfield(L, -2, "args");

    std::error_code ec;
    set_text(L, "cwd", std::filesystem::current_path(ec).string());
    set_flag(L, "interactive", config_.interactive);
    set_flag(L, "quiet", config_.quiet);
    set_flag(L, "sandboxed", sandboxed_);
    set_text(L, "default_loop", config_.default_loop);
    set_flag(L, "tty", isatty(STDOUT_FILENO) != 0);
    set_text(L, "model", config_.model);
    lua_createtable(L, 0, static_cast<int>(http_->endpoints().size()));
    for (const Endpoint& each : http_->endpoints()) {
        set_flag(L, each.name.c_str(), each.key_env.empty() || !each.key.empty());
    }
    lua_setreadonly(L, -1, 1);
    lua_setfield(L, -2, "endpoints");
    set_flag(L, "judged", judge_->valid());
    if (!config_.workflow.empty()) {
        set_text(L, "workflow", config_.workflow);
    }
    if (!config_.resume.empty()) {
        set_text(L, "resume", config_.resume);
    }
    for (const auto& [field, texts] :
         {std::pair{"extensions", &config_.extensions}, std::pair{"skills", &config_.skills}}) {
        lua_createtable(L, 0, static_cast<int>(texts->size()));
        for (const auto& [name, text] : *texts) {
            set_text(L, name.c_str(), text);
        }
        lua_setreadonly(L, -1, 1);
        lua_setfield(L, -2, field);
    }
    lua_pop(L, 1);
}

}  // namespace hx
