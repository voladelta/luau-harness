#ifndef HX_HOST_H_
#define HX_HOST_H_

#include <cstdint>
#include <memory>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "sandbox.h"
#include "sys.h"

struct lua_State;

namespace hx {

class HttpClient;
class JudgeChannel;
class SpawnPool;

enum class Auth : std::uint8_t {
    ApiKey,
    Bearer,
    GoogleKey,
};

struct Endpoint {
    std::string name;
    std::string url;
    Auth auth = Auth::ApiKey;
    std::string key;
    std::string key_env;
};

inline constexpr std::string_view kSessionEndpoint = "session";

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
    std::string resume;
};

class Host {
  public:
    Host(HostConfig config, const Sandbox& sandbox, bool* memory_refused = nullptr);
    Host(const Host&) = delete;
    Host& operator=(const Host&) = delete;
    ~Host();

    void open(lua_State* L);
    static void check_deadline(lua_State* L);

  private:
    static Host& of(lua_State* L);
    static void on_interrupt(lua_State* L, int gc);
    static int post(lua_State* L);
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
    static int stop(lua_State* L);

    HostConfig config_;
    bool sandboxed_;
    bool* memory_refused_;
    double deadline_ = 0;
    int tries_ = 0;
    std::unique_ptr<HttpClient> http_;
    std::unique_ptr<JudgeChannel> judge_;
    std::unique_ptr<SpawnPool> spawn_;
};

void install_signal_handlers();
int stop_signal(std::string_view error) noexcept;

}  // namespace hx

#endif  // HX_HOST_H_
