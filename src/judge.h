#ifndef HX_JUDGE_H_
#define HX_JUDGE_H_

#include <sys/types.h>

#include <filesystem>
#include <functional>
#include <string>
#include <vector>

#include "sandbox.h"

namespace hx {

// With --judge, a second process measures what the agent submits. It is forked before the agent's wall goes up and
// puts up a wall of its own: it reads the project and the kit and writes only its state directory, which the
// agent's wall leaves out, as it leaves out the kit. The agent's only way to it is one socket, line by line
// (host.judge). The judge itself is lua/judge.luau, built into hx, so nothing the agent writes can change it.

// What the judge needs to know about the agent's walls, as plain data.
struct JudgeWalls {
    std::vector<Rule> system;  // what every wall lets in
    std::vector<Rule> user;    // the --ro and --rw paths, which the candidate's commands get too
    std::vector<Rule> agent;   // the agent's whole wall, which must reach neither the kit nor the judge's state
    bool unix_sockets = false;
};

// Runs lua/judge.luau in a fresh VM, behind `wall`, with `args` as the script's arguments; returns its exit status.
// Called in the judge's process, after its wall is up.
using JudgeRunner = std::function<int(std::vector<std::string> args, const Sandbox& wall)>;

struct Judge {
    pid_t pid = -1;
    int channel = -1;  // the agent's end, owned by the Host
    std::filesystem::path state;
};

// Forks the judge for --judge KIT, waits until it has measured the starting point, and returns the agent's end
// of the channel to it. Throws UsageError when the kit or the walls are wrong.
[[nodiscard]] Judge spawn_judge(const std::string& kit_dir, const std::filesystem::path& project,
                                const JudgeWalls& walls, const JudgeRunner& run);

// Tells the judge the session is over and waits for its summary.
void finish_judge(const Judge& judge);

}  // namespace hx

#endif  // HX_JUDGE_H_
