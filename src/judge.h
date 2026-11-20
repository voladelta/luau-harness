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
            finish();
            pid_ = std::exchange(other.pid_, -1);
            channel_ = std::move(other.channel_);
            state_ = std::move(other.state_);
        }
        return *this;
    }
    ~Judge() { finish(); }

    [[nodiscard]] int channel() const noexcept { return channel_.get(); }
    [[nodiscard]] UniqueFd take_channel() noexcept { return std::move(channel_); }
    void finish(int external_channel = -1) noexcept;

  private:
    pid_t pid_ = -1;
    UniqueFd channel_;
    std::filesystem::path state_;
};

[[nodiscard]] Judge spawn_judge(const std::string& kit_dir, const std::filesystem::path& project,
                                const JudgeWalls& walls, const JudgeRunner& run);

}  // namespace hx

#endif  // HX_JUDGE_H_
