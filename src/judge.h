#ifndef HX_JUDGE_H_
#define HX_JUDGE_H_

#include <sys/types.h>

#include <filesystem>
#include <functional>
#include <string>
#include <utility>
#include <vector>

#include "sandbox.h"
#include "sys.h"

namespace hx {

struct JudgeWalls {
    std::vector<Rule> system;
    std::vector<Rule> user;
    std::vector<Rule> agent;
    bool unix_sockets = false;
};

using JudgeRunner = std::function<int(std::vector<std::string> args, const Sandbox& wall)>;

class Judge {
  public:
    Judge() = default;
    Judge(pid_t pid, UniqueFd channel, std::filesystem::path state) noexcept
        : pid_(pid), channel_(std::move(channel)), state_(std::move(state)) {}
    Judge(const Judge&) = delete;
    Judge& operator=(const Judge&) = delete;
    Judge(Judge&& other) noexcept
        : pid_(std::exchange(other.pid_, -1)), channel_(std::move(other.channel_)), state_(std::move(other.state_)) {}
    Judge& operator=(Judge&& other) noexcept {
        if (this != &other) {
            static_cast<void>(stop());
            pid_ = std::exchange(other.pid_, -1);
            channel_ = std::move(other.channel_);
            state_ = std::move(other.state_);
        }
        return *this;
    }
    ~Judge() { static_cast<void>(stop()); }

    [[nodiscard]] UniqueFd share_channel() const;
    void finish();

  private:
    [[nodiscard]] bool stop() noexcept;

    pid_t pid_ = -1;
    UniqueFd channel_;
    std::filesystem::path state_;
};

[[nodiscard]] Judge spawn_judge(const std::string& kit_dir, const std::filesystem::path& project,
                                const JudgeWalls& walls, const JudgeRunner& run);

}  // namespace hx

#endif  // HX_JUDGE_H_
