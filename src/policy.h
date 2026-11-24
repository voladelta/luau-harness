#ifndef HX_POLICY_H_
#define HX_POLICY_H_

#include <filesystem>
#include <string>
#include <vector>

#include "judge.h"
#include "sandbox.h"

namespace hx {

class WallPolicy {
  public:
    WallPolicy(std::filesystem::path project, std::vector<std::string> read_paths, std::vector<std::string> write_paths,
               bool unix_sockets);

    void check() const;
    [[nodiscard]] std::vector<Rule> system_rules() const;
    [[nodiscard]] Sandbox confine(const std::filesystem::path& tmp) const;
    [[nodiscard]] JudgeWalls judge_walls(const std::filesystem::path& tmp) const;
    [[nodiscard]] std::string unwritable_path() const;

  private:
    [[nodiscard]] std::vector<std::filesystem::path> writable_roots() const;
    [[nodiscard]] std::vector<Rule> user_rules() const;
    [[nodiscard]] std::vector<Rule> settled_user_rules() const;
    [[nodiscard]] std::vector<Rule> agent_rules(const std::filesystem::path& tmp) const;

    std::filesystem::path project_;
    std::vector<std::string> read_paths_;
    std::vector<std::string> write_paths_;
    bool unix_sockets_;
};

}  // namespace hx

#endif  // HX_POLICY_H_
