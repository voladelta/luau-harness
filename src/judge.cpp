#include "judge.h"

#include <fcntl.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <ctime>
#include <exception>
#include <filesystem>
#include <stdexcept>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <fmt/format.h>

#include "sandbox.h"
#include "sys.h"
#include "usage.h"

namespace hx {
namespace {

namespace fs = std::filesystem;

bool reaches(const std::vector<Rule>& rules, const fs::path& path) {
    return std::any_of(rules.begin(), rules.end(), [&](const Rule& rule) {
        std::error_code ec;
        const fs::path dir = fs::weakly_canonical(rule.path, ec);
        return !ec && beneath(path, dir);
    });
}

std::string_view flag_of(const Rule& rule) {
    return rule.access == Access::Write ? "--rw" : "--ro";
}

fs::path make_judge_state(const fs::path& project) {
    std::string base = env_or("XDG_STATE_HOME");
    if (!base.starts_with('/')) {
        base = env_or("HOME") + "/.local/state";
    }
    const fs::path parent = fs::path(base) / "hx" / "judge";
    std::error_code ec;
    fs::create_directories(parent, ec);
    const std::time_t now = std::time(nullptr);
    std::tm local{};
    localtime_r(&now, &local);
    std::array<char, 32> stamp{};
    const std::size_t length = std::strftime(stamp.data(), stamp.size(), "%Y%m%d-%H%M%S", &local);
    std::string pattern =
        (parent / fmt::format("{}-{}-XXXXXX", project.filename().string(), std::string_view(stamp.data(), length)))
            .string();
    if (ec || mkdtemp(pattern.data()) == nullptr) {
        throw UsageError(fmt::format("cannot make the judge's state directory in {}: {}", parent.string(),
                                     ec ? ec.message() : error_text()));
    }
    return fs::canonical(pattern);
}

[[noreturn]] void judge_main(const JudgeWalls& walls, const JudgeRunner& run, int channel, const fs::path& project,
                             const fs::path& kit, const fs::path& state) {
    int status = kExitFailure;
    try {
        setsid();
        if (dup2(channel, STDIN_FILENO) < 0 || dup2(channel, STDOUT_FILENO) < 0) {
            throw std::runtime_error(fmt::format("judge: dup2: {}", error_text()));
        }
        close(channel);
        static_cast<void>(std::setvbuf(stdout, nullptr, _IOLBF, 0));
        const fs::path tmp = state / "tmp";
        fs::create_directories(tmp);
        setenv("TMPDIR", tmp.c_str(), 1);  // NOLINT(concurrency-mt-unsafe)

        std::vector<Rule> rules = walls.system;
        rules.push_back({.path = project.string(), .access = Access::Read, .required = true});
        rules.push_back({.path = kit.string(), .access = Access::Read, .required = true});
        rules.push_back({.path = state.string(), .access = Access::Write, .required = true});
        rules.insert(rules.end(), walls.user.begin(), walls.user.end());
        const Sandbox sandbox = Sandbox::confine(rules, walls.unix_sockets);
        prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);

        std::vector<std::string> args = {kit.string(), state.string()};
        for (const Rule& rule : walls.user) {
            args.insert(args.end(), {std::string(flag_of(rule)), rule.path});
        }
        status = run(std::move(args), sandbox);
    } catch (const std::exception& error) {
        fmt::print(stderr, "hx: judge: {}\n", error.what());
    }
    static_cast<void>(std::fflush(stderr));
    _exit(status);
}

void check_kit(const fs::path& kit) {
    for (const char* name : {"run", "build", "check"}) {
        const fs::path script = kit / name;
        const bool required = std::string_view(name) == "run";
        if ((required || fs::exists(script)) && (!fs::is_regular_file(script) || access(script.c_str(), X_OK) != 0)) {
            throw UsageError(fmt::format("--judge {}: {} must be an executable file", kit.string(), name));
        }
    }
    if (fs::exists(kit / "kit.json") && !fs::is_regular_file(kit / "kit.json")) {
        throw UsageError(fmt::format("--judge {}: kit.json must be a file", kit.string()));
    }
}

void await_judge(int channel) {
    std::string line;
    char c = 0;
    while (line.size() < 4096) {
        const ssize_t n = read(channel, &c, 1);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n <= 0 || c == '\n') {
            break;
        }
        line += c;
    }
    if (line != "ready") {
        throw UsageError(line.empty() ? "the judge did not start" : fmt::format("the judge cannot start: {}", line));
    }
}

}  // namespace

Judge spawn_judge(const std::string& kit_dir, const fs::path& project, const JudgeWalls& walls,
                  const JudgeRunner& run) {
    std::error_code ec;
    const fs::path kit = fs::canonical(kit_dir, ec);
    if (ec || !fs::is_directory(kit)) {
        throw UsageError(fmt::format("--judge {}: not a directory", kit_dir));
    }
    check_kit(kit);
    for (const Rule& rule : walls.user) {
        if (!fs::exists(rule.path, ec)) {
            throw UsageError(fmt::format("{} {}: no such file or directory", flag_of(rule), rule.path));
        }
    }
    if (reaches(walls.agent, kit)) {
        throw UsageError(
            fmt::format("--judge {}: the agent could read the kit there; keep it outside the project, the "
                        "system directories and every --ro and --rw path",
                        kit.string()));
    }
    fs::path state = make_judge_state(project);
    if (reaches(walls.agent, state)) {
        fs::remove(state, ec);
        throw UsageError(
            fmt::format("the judge's state directory {} would be within the agent's reach", state.string()));
    }
    std::array<int, 2> ends{};
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, ends.data()) != 0) {
        throw std::runtime_error(fmt::format("socketpair: {}", error_text()));
    }
    UniqueFd parent_end(ends[0]);
    UniqueFd child_end(ends[1]);
    static_cast<void>(std::fflush(nullptr));
    const pid_t pid = fork();
    if (pid < 0) {
        throw std::runtime_error(fmt::format("fork: {}", error_text()));
    }
    if (pid == 0) {
        parent_end.reset();
        judge_main(walls, run, child_end.get(), project, kit, state);
    }
    child_end.reset();
    const int channel = parent_end.get();
    Judge judge(pid, std::move(parent_end), std::move(state));
    await_judge(channel);
    return judge;
}

UniqueFd Judge::share_channel() const {
    UniqueFd shared(fcntl(channel_.get(), F_DUPFD_CLOEXEC, 0));
    if (!shared.valid()) {
        throw std::runtime_error(fmt::format("judge channel: {}", error_text()));
    }
    return shared;
}

bool Judge::stop() noexcept {
    if (pid_ < 0) {
        return true;
    }
    const pid_t pid = std::exchange(pid_, -1);
    if (channel_.valid()) {
        shutdown(channel_.get(), SHUT_WR);
        channel_.reset();
    }
    constexpr int kWaitTicks = 300;
    for (int tick = 0; tick < kWaitTicks; ++tick) {
        const pid_t done = waitpid(pid, nullptr, WNOHANG);
        if (done == pid || (done < 0 && errno == ECHILD)) {
            return true;
        }
        nap(100);
    }
    return false;
}

void Judge::finish() {
    if (!stop()) {
        fmt::print(stderr, "hx: the judge did not finish; its state is in {}\n", state_.string());
    }
}

}  // namespace hx
