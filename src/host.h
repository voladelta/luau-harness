#ifndef HX_HOST_H_
#define HX_HOST_H_

#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "sandbox.h"
#include "sys.h"

struct lua_State;
using CURL = void;

namespace hx {

enum class Auth : std::uint8_t {
    ApiKey,
    Bearer,
};

struct Endpoint {
    std::string name;
    std::string url;
    Auth auth = Auth::ApiKey;
    std::string key;
    std::string key_env;
    std::vector<std::string> headers;
    bool form = false;
    bool model = false;
};

struct HostConfig {
    std::vector<Endpoint> endpoints;
    std::string model;
    std::string ca_bundle;
    std::vector<std::string> args;
    bool interactive = false;
    bool quiet = false;
    std::string default_loop;
    std::vector<std::pair<std::string, std::string>> extensions;
    std::vector<std::pair<std::string, std::string>> skills;
    std::vector<Rule> system_rules;
    UniqueFd judge;
    std::string workflow;
};

class Host {
  public:
    Host(HostConfig config, const Sandbox& sandbox);
    Host(const Host&) = delete;
    Host& operator=(const Host&) = delete;
    ~Host();

    void open(lua_State* L);

  private:
    static Host& of(lua_State* L);
    static void on_interrupt(lua_State* L, int gc);
    static int llm(lua_State* L);
    static int tool(lua_State* L);
    static int sh(lua_State* L);
    static int try_(lua_State* L);
    static int timed(lua_State* L);
    static int protect(lua_State* L, int fn, double deadline);
    static int sleep(lua_State* L);
    static int read_file(lua_State* L);
    static int write_file(lua_State* L);
    static int judge(lua_State* L);
    static int spawn(lua_State* L);
    static int wait(lua_State* L);
    [[noreturn]] void run_child(lua_State* L, int result_fd) noexcept;

    [[nodiscard]] double within_limit(double seconds) const;
    [[nodiscard]] std::optional<Wall> narrower_wall(lua_State* L, int idx) const;
    [[nodiscard]] const Endpoint* endpoint(std::string_view name) const;
    CURL* curl();

    struct CurlDeleter {
        void operator()(CURL* handle) const noexcept;
    };

    struct Spawned {
        int id = 0;
        UniqueFd result;
        std::string data;
        bool ended = false;
    };
    static void drain(Spawned& child);

    HostConfig config_;
    bool sandboxed_;
    std::unique_ptr<CURL, CurlDeleter> curl_;
    double deadline_ = 0;
    int tries_ = 0;
    std::string judge_in_;
    int judge_owed_ = 0;
    std::vector<Spawned> spawned_;
    int spawn_count_ = 0;
    bool child_ = false;
};

void install_signal_handlers();

void vm_memory_refused() noexcept;

}  // namespace hx

#endif  // HX_HOST_H_
