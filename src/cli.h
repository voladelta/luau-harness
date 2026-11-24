#ifndef HX_CLI_H_
#define HX_CLI_H_

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "host.h"

namespace hx {

enum class Mode : std::uint8_t { Agent, Script, Init, Help, Version };

struct Options {
    Mode mode = Mode::Agent;
    std::string script;
    std::vector<std::string> args;
    std::vector<std::string> read_paths;
    std::vector<std::string> write_paths;
    std::vector<std::string> keep_env;
    std::optional<std::string> model;
    std::optional<std::string> judge;
    std::optional<std::string> resume;
    std::vector<Endpoint> endpoints;
    bool rpi = false;
    bool built_in_loop = false;
    bool sandbox = true;
    bool unix_sockets = false;
    bool full = false;
    bool quiet = false;
    bool no_input = false;
};

[[nodiscard]] std::string_view usage() noexcept;
[[nodiscard]] Options parse_args(const std::vector<std::string_view>& argv);
int init_project(bool full);
[[nodiscard]] HostConfig build_host_config(const Options& options);
[[nodiscard]] std::optional<std::string> read_script(const Options& options);

}  // namespace hx

#endif  // HX_CLI_H_
