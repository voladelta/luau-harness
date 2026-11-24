#ifndef HX_HOST_INTERNAL_H_
#define HX_HOST_INTERNAL_H_

#include <poll.h>
#include <sys/types.h>

#include <csignal>
#include <functional>
#include <memory>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include "host.h"
#include "lua.h"
#include "sandbox.h"
#include "sys.h"

#if LUA_USE_LONGJMP
#error "hx needs Luau built to raise errors as C++ exceptions, not with longjmp"
#endif

using CURL = void;

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
double within_limit(double deadline, double seconds);
const char* stop_error(int sig);
[[noreturn]] void raise_error(lua_State* L, const char* error);
void raise_if_signaled(lua_State* L);
void push_strings(lua_State* L, const std::vector<std::string>& strings);
void renew_limit_timer() noexcept;

std::string_view cut_head(std::string_view head);
std::string_view cut_tail(std::string_view tail);

int read_file_lua(lua_State* L, double deadline);
int write_file_lua(lua_State* L, double deadline);
int l_list(lua_State* L);
int l_stat(lua_State* L);
int l_walk(lua_State* L);

int run_sh(lua_State* L, double deadline, const std::vector<Rule>& system_rules);

class HttpClient {
  public:
    HttpClient(std::vector<Endpoint> endpoints, std::string model, std::string ca_bundle);
    HttpClient(const HttpClient&) = delete;
    HttpClient& operator=(const HttpClient&) = delete;
    ~HttpClient();

    [[nodiscard]] const std::vector<Endpoint>& endpoints() const noexcept { return endpoints_; }
    void reset_handle() noexcept { curl_.reset(); }

    int route(lua_State* L) const;
    int post(lua_State* L, double deadline);

  private:
    struct CurlDeleter {
        void operator()(CURL* handle) const noexcept;
    };

    [[nodiscard]] const Endpoint* endpoint(std::string_view name) const;
    CURL* curl();

    std::vector<Endpoint> endpoints_;
    std::string model_;
    std::string ca_bundle_;
    std::unique_ptr<CURL, CurlDeleter> curl_;
};

class JudgeChannel {
  public:
    explicit JudgeChannel(UniqueFd fd) noexcept : fd_(std::move(fd)) {}
    JudgeChannel(const JudgeChannel&) = delete;
    JudgeChannel& operator=(const JudgeChannel&) = delete;
    ~JudgeChannel() = default;

    [[nodiscard]] bool valid() const noexcept { return fd_.valid(); }
    void reset() noexcept { fd_.reset(); }

    int exchange(lua_State* L, double deadline);

  private:
    UniqueFd fd_;
    std::string in_;
    int owed_ = 0;
};

class SpawnPool {
  public:
    SpawnPool() = default;
    SpawnPool(const SpawnPool&) = delete;
    SpawnPool& operator=(const SpawnPool&) = delete;
    ~SpawnPool() = default;

    int spawn(lua_State* L, const std::function<void()>& before_fork, const std::function<void()>& in_child);
    int wait(lua_State* L, double deadline);
    int stop(lua_State* L);

  private:
    struct Spawned {
        int id = 0;
        pid_t pid = 0;
        UniqueFd result;
        std::string data;
        bool ended = false;
    };

    static void drain(Spawned& child);
    [[noreturn]] void run_child(lua_State* L, int result_fd, const std::function<void()>& in_child) noexcept;

    std::vector<Spawned> spawned_;
    int count_ = 0;
    bool child_ = false;
};

double now() noexcept;
void wait_for(std::span<pollfd> fds, double seconds) noexcept;
void kill_descendants(std::span<const pid_t> spared = {}) noexcept;
std::vector<pid_t> descendants_now();
void reap_exited() noexcept;

}  // namespace hx

#endif  // HX_HOST_INTERNAL_H_
