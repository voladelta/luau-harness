#include "vm.h"

#include <fcntl.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <charconv>
#include <csignal>
#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>

#include <fmt/format.h>

#include "embedded.h"
#include "json.h"
#include "lua.h"
#include "luacode.h"
#include "lualib.h"
#include "sys.h"
#include "usage.h"

namespace hx {
namespace {

constexpr int kExitInterrupted = 130;
constexpr int kExitTerminated = 143;

constexpr std::size_t kMaxVmMemory = std::size_t{1} << 30;
constexpr std::size_t kVmStep = std::size_t{64} << 20;
constexpr std::size_t kSystemReserve = std::size_t{128} << 20;

std::optional<std::size_t> system_available() noexcept {
    const UniqueFd file(open("/proc/meminfo", O_RDONLY | O_CLOEXEC));
    std::array<char, 4096> buffer{};
    const ssize_t n = file.valid() ? read(file.get(), buffer.data(), buffer.size()) : -1;
    if (n <= 0) {
        return std::nullopt;
    }
    const std::string_view text(buffer.data(), static_cast<std::size_t>(n));
    constexpr std::string_view kField = "MemAvailable:";
    std::size_t at = text.find(kField);
    if (at == std::string_view::npos) {
        return std::nullopt;
    }
    at = text.find_first_not_of(' ', at + kField.size());
    if (at == std::string_view::npos) {
        return std::nullopt;
    }
    std::size_t kib = 0;
    const auto [end, error] = std::from_chars(text.data() + at, text.data() + text.size(), kib);
    if (error != std::errc()) {
        return std::nullopt;
    }
    return kib * 1024;
}

struct VmMemory {
    std::size_t used = 0;
    std::size_t granted = 0;
    bool refused = false;
};

bool grant(VmMemory& memory, std::size_t growth) noexcept {
    const std::size_t need = memory.used + growth;
    if (need <= memory.granted) {
        return true;
    }
    if (need > kMaxVmMemory) {
        return false;
    }
    const std::optional<std::size_t> available = system_available();
    if (!available) {
        memory.granted = kMaxVmMemory;
        return true;
    }
    const std::size_t room = *available > kSystemReserve ? *available - kSystemReserve : 0;
    if (growth > room) {
        return false;
    }
    memory.granted = std::min(kMaxVmMemory, memory.used + std::min(room, std::max(growth, kVmStep)));
    return true;
}

// NOLINTBEGIN(cppcoreguidelines-no-malloc)
void* limited_alloc(void* user, void* block, std::size_t old_size, std::size_t new_size) {
    auto& memory = *static_cast<VmMemory*>(user);
    std::size_t& used = memory.used;
    if (new_size == 0) {
        std::free(block);
        used -= old_size;
        memory.granted = std::min(memory.granted, used + kVmStep);
        return nullptr;
    }
    if (new_size > old_size && !grant(memory, new_size - old_size)) {
        memory.refused = true;
        return nullptr;
    }
    void* const grown = std::realloc(block, new_size);  // NOLINT(misc-const-correctness)
    if (grown != nullptr) {
        used = used - old_size + new_size;
    }
    return grown;
}
// NOLINTEND(cppcoreguidelines-no-malloc)

struct StateCloser {
    void operator()(lua_State* L) const noexcept { lua_close(L); }
};
using LuaState = std::unique_ptr<lua_State, StateCloser>;
using Bytecode = std::unique_ptr<char, FreeDeleter>;

bool load_chunk(lua_State* L, std::string_view source, const std::string& name) {
    std::size_t size = 0;
    const Bytecode bytecode(luau_compile(source.data(), source.size(), nullptr, &size));
    if (!bytecode) {
        lua_pushstring(L, "out of memory");
        return false;
    }
    return luau_load(L, name.c_str(), bytecode.get(), size, 0) == 0;
}

std::string pop_message(lua_State* L) {
    std::size_t length = 0;
    const char* text = lua_tolstring(L, -1, &length);
    std::string message = text != nullptr ? std::string(text, length) : std::string("(error is not a string)");
    lua_pop(L, 1);
    return message;
}

bool is_stop(lua_State* L) {
    const char* text = lua_tostring(L, -1);
    return text != nullptr && stop_signal(text) != 0;
}

int failure(lua_State* L) {
    const std::string message = pop_message(L);
    if (const int sig = stop_signal(message); sig != 0) {
        return sig == SIGINT ? kExitInterrupted : kExitTerminated;
    }
    fmt::print(stderr, "hx: {}\n", message);
    return kExitFailure;
}

bool is_module(lua_State* L) {
    if (!lua_istable(L, -1)) {
        return false;
    }
    lua_getfield(L, -1, "main");
    const bool module = lua_isfunction(L, -1);
    lua_pop(L, 1);
    return module;
}

int run_main(lua_State* L) {
    lua_getfield(L, -1, "main");
    if (lua_pcall(L, 0, 1, 0) != 0) {
        return failure(L);
    }
    return lua_isnumber(L, -1) != 0 ? lua_tointeger(L, -1) : 0;
}

int execute_module(lua_State* L, std::string_view source, const std::string& name) {
    if (!load_chunk(L, source, name) || lua_pcall(L, 0, 1, 0) != 0) {
        return failure(L);
    }
    return is_module(L) ? run_main(L) : 0;
}

int execute_loop(lua_State* L, bool built_in) {
    std::error_code ec;
    if (built_in) {
        return execute_module(L, kDefaultLoop, "=loop");
    }
    if (const std::string project = read_file(kProjectLoop, ec); ec != std::errc::no_such_file_or_directory) {
        std::string why;
        if (ec) {
            why = fmt::format("cannot be read ({})", ec.message());
        } else if (!load_chunk(L, project, fmt::format("={}", kProjectLoop))) {
            why = fmt::format("does not compile ({})", pop_message(L));
        } else if (lua_pcall(L, 0, 1, 0) != 0) {
            if (is_stop(L)) {
                return failure(L);
            }
            why = fmt::format("raised an error while loading ({})", pop_message(L));
        } else if (!is_module(L)) {
            lua_pop(L, 1);
            why = "does not return a table with a main function";
        } else {
            return run_main(L);
        }
        fmt::print(stderr, "hx: {} {}. Fix it, or pass --built-in-loop to run without it.\n", kProjectLoop, why);
        return kExitUsage;
    }
    return execute_module(L, kDefaultLoop, "=loop");
}

void freeze_libraries(lua_State* L) {
    lua_pushnil(L);
    while (lua_next(L, LUA_GLOBALSINDEX) != 0) {
        if (lua_istable(L, -1) && lua_rawequal(L, -1, LUA_GLOBALSINDEX) == 0) {
            lua_setreadonly(L, -1, 1);
        }
        lua_pop(L, 1);
    }
    lua_pushliteral(L, "");
    if (lua_getmetatable(L, -1) != 0) {
        lua_setreadonly(L, -1, 1);
        lua_pop(L, 1);
    }
    lua_pop(L, 1);
}

template <typename Body>
int run_vm(HostConfig config, const Sandbox& sandbox, const Body& body, const std::function<void()>& on_done) {
    install_signal_handlers();
    VmMemory vm_memory;
    Host host(std::move(config), sandbox, &vm_memory.refused);
    const LuaState state(lua_newstate(limited_alloc, &vm_memory));
    if (!state) {
        throw std::runtime_error("cannot create a Luau state");
    }
    lua_State* L = state.get();
    luaL_openlibs(L);
    open_json(L);
    freeze_libraries(L);
    host.open(L);
    const int status = body(L);
    if (on_done) {
        on_done();
    }
    return status;
}

}  // namespace

int run_module(HostConfig config, const Sandbox& sandbox, std::string_view source, const std::string& name,
               const std::function<void()>& on_done) {
    return run_vm(std::move(config), sandbox, [&](lua_State* L) { return execute_module(L, source, name); }, on_done);
}

int run_loop(HostConfig config, const Sandbox& sandbox, bool built_in, const std::function<void()>& on_done) {
    return run_vm(std::move(config), sandbox, [&](lua_State* L) { return execute_loop(L, built_in); }, on_done);
}

}  // namespace hx
