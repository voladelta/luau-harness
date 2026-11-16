#ifndef HX_SANDBOX_H_
#define HX_SANDBOX_H_

#include <cstdint>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>

#include "sys.h"

namespace hx {

enum class Access : std::uint8_t {
    Read,
    Write,
    Device,
};

struct Rule {
    std::string path;
    Access access;
    bool required;
};

class SandboxError : public std::runtime_error {
  public:
    using std::runtime_error::runtime_error;
};

class Sandbox {
  public:
    [[nodiscard]] static Sandbox confine(const std::vector<Rule>& rules, bool unix_sockets);

    [[nodiscard]] static Sandbox none() noexcept { return Sandbox(0, false); }

    static constexpr int kUnixSocketAbi = 9;

    [[nodiscard]] bool confined() const noexcept { return abi_ > 0; }
    [[nodiscard]] bool blocks_unix_sockets() const noexcept { return abi_ >= kUnixSocketAbi || filtered_; }
    [[nodiscard]] int abi() const noexcept { return abi_; }

  private:
    explicit Sandbox(int abi, bool filtered) noexcept : abi_(abi), filtered_(filtered) {}

    int abi_;
    bool filtered_;
};

class Wall {
  public:
    [[nodiscard]] static Wall prepare(const std::vector<Rule>& rules);

    [[nodiscard]] bool enter() const noexcept;

  private:
    explicit Wall(UniqueFd ruleset) noexcept : ruleset_(std::move(ruleset)) {}

    UniqueFd ruleset_;
};

}  // namespace hx

#endif  // HX_SANDBOX_H_
