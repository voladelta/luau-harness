#ifndef HX_JUDGE_H_
#define HX_JUDGE_H_

#include <sys/types.h>

#include <filesystem>
#include <functional>
#include <string>
#include <vector>

#include "sandbox.h"

namespace hx {

struct JudgeWalls {
    std::vector<Rule> system;
    std::vector<Rule> user;
    std::vector<Rule> agent;
    bool unix_sockets = false;
};

using JudgeRunner = std::function<int(std::vector<std::string> args, const Sandbox& wall)>;

struct Judge {
    pid_t pid = -1;
    int channel = -1;
    std::filesystem::path state;
};

[[nodiscard]] Judge spawn_judge(const std::string& kit_dir, const std::filesystem::path& project,
                                const JudgeWalls& walls, const JudgeRunner& run);

void finish_judge(const Judge& judge);

}  // namespace hx

#endif  // HX_JUDGE_H_
