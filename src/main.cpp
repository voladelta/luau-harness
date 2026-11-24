#include <curl/curl.h>
#include <poll.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <exception>
#include <filesystem>
#include <optional>
#include <stdexcept>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <fmt/format.h>

#include "cli.h"
#include "embedded.h"
#include "host.h"
#include "judge.h"
#include "policy.h"
#include "sandbox.h"
#include "sys.h"
#include "usage.h"
#include "vm.h"

namespace {

namespace fs = std::filesystem;

using hx::kExitFailure;
using hx::kExitUsage;
using hx::UsageError;

class TempDir {
  public:
    TempDir() {
        std::error_code ec;
        std::string pattern = (fs::temp_directory_path(ec) / "hx-XXXXXX").string();
        if (ec || mkdtemp(pattern.data()) == nullptr) {
            throw UsageError(fmt::format("cannot create a temporary directory: {}", hx::error_text()));
        }
        path_ = std::move(pattern);
        setenv("TMPDIR", path_.c_str(), 1);  // NOLINT(concurrency-mt-unsafe)
        remover_ = start_remover(path_.parent_path().c_str(), path_.filename().c_str());
    }
    TempDir(const TempDir&) = delete;
    TempDir& operator=(const TempDir&) = delete;
    ~TempDir() {
        std::error_code ec;
        fs::remove_all(path_, ec);
        if (shutdown(remover_.get(), SHUT_WR) == 0) {
            pollfd gone{.fd = remover_.get(), .events = POLLIN, .revents = 0};
            static_cast<void>(poll(&gone, 1, kRemoverWaitMs));
        }
    }

    [[nodiscard]] const fs::path& path() const noexcept { return path_; }

  private:
    static constexpr int kRemoverWaitMs = 1000;

    static hx::UniqueFd start_remover(const char* parent, const char* name) noexcept {
        std::array<int, 2> ends{};
        if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, ends.data()) != 0) {
            return {};
        }
        hx::UniqueFd ours(ends[0]);
        const hx::UniqueFd theirs(ends[1]);
        const pid_t pid = fork();
        if (pid == 0) {
            const pid_t remover = chdir(parent) == 0 ? fork() : -1;
            if (remover != 0) {
                _exit(remover > 0 ? 0 : 1);
            }
            setsid();
            prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
            prctl(PR_SET_NAME, "hx-tmpdir", 0, 0, 0);
            dup2(theirs.get(), STDIN_FILENO);
            static_cast<void>(close_range(STDIN_FILENO + 1, ~0U, 0));
            char byte = 0;
            ssize_t got = 0;
            do {
                got = read(STDIN_FILENO, &byte, 1);
            } while (got > 0 || (got < 0 && errno == EINTR));
            std::error_code ec;
            fs::remove_all(name, ec);
            _exit(0);
        }
        int status = -1;
        while (pid > 0 && waitpid(pid, &status, 0) < 0 && errno == EINTR) {
        }
        return status == 0 ? std::move(ours) : hx::UniqueFd();
    }

    fs::path path_;
    hx::UniqueFd remover_;
};

class CurlGlobal {
  public:
    CurlGlobal() {
        if (curl_global_init(CURL_GLOBAL_DEFAULT) != CURLE_OK) {
            throw std::runtime_error("cannot initialise libcurl");
        }
    }
    CurlGlobal(const CurlGlobal&) = delete;
    CurlGlobal& operator=(const CurlGlobal&) = delete;
    ~CurlGlobal() { curl_global_cleanup(); }
};

void close_inherited_fds() {
    if (hx::cloexec_above_stderr() != 0) {
        throw std::runtime_error(fmt::format("cannot mark inherited descriptors close-on-exec: {}", hx::error_text()));
    }
}

hx::Judge spawn_judge(const hx::WallPolicy& policy, const hx::Options& options, const std::string& kit_dir,
                      const fs::path& project, const fs::path& tmp) {
    const std::string path = policy.unwritable_path();
    const hx::JudgeWalls walls = policy.judge_walls(tmp);
    return hx::spawn_judge(kit_dir, project, walls, [&](std::vector<std::string> args, const hx::Sandbox& wall) {
        setenv("PATH", path.c_str(), 1);  // NOLINT(concurrency-mt-unsafe)
        hx::HostConfig config;
        config.args = std::move(args);
        config.quiet = options.quiet;
        config.system_rules = policy.system_rules();
        return hx::run_module(std::move(config), wall, hx::kJudge, "=judge");
    });
}

int run(const std::vector<std::string_view>& argv) {
    close_inherited_fds();
    const hx::Options options = hx::parse_args(argv);
    if (options.mode == hx::Mode::Help) {
        fmt::print("{}", hx::usage());
        return 0;
    }
    if (options.mode == hx::Mode::Version) {
        fmt::print("hx {}\n", hx::kVersion);
        return 0;
    }
    if (options.mode == hx::Mode::Init) {
        return hx::init_project(options.full);
    }

    hx::HostConfig config = hx::build_host_config(options);
    const std::optional<std::string> script = hx::read_script(options);

    std::error_code ec;
    const fs::path project = fs::canonical(fs::current_path(ec), ec);
    if (ec) {
        throw UsageError(fmt::format("cannot resolve the current directory: {}", ec.message()));
    }
    const hx::WallPolicy policy(project, options.read_paths, options.write_paths, options.unix_sockets);
    const TempDir tmp;

    static_cast<void>(std::setvbuf(stdout, nullptr, _IOLBF, 0));
    if (options.judge) {
        policy.check();
    }
    std::optional<hx::Judge> judge =
        options.judge ? std::optional(spawn_judge(policy, options, *options.judge, project, tmp.path())) : std::nullopt;
    const CurlGlobal curl;
    const hx::Sandbox sandbox = options.sandbox ? policy.confine(tmp.path()) : hx::Sandbox::none();
    if (sandbox.confined() && !sandbox.blocks_unix_sockets()) {
        fmt::print(stderr,
                   "hx: note: this kernel's Landlock (ABI {}) cannot stop commands from connecting to unix sockets "
                   "such as the D-Bus session bus or ssh-agent; ABI {} can. See docs/wall.md.\n",
                   sandbox.abi(), hx::Sandbox::kUnixSocketAbi);
    }
    prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
    config.system_rules = policy.system_rules();
    if (judge) {
        config.judge = judge->share_channel();
    }
    const auto finish_judge = [&] {
        if (judge) {
            judge->finish();
        }
    };
    if (script) {
        return hx::run_module(std::move(config), sandbox, *script, fmt::format("={}", options.script), finish_judge);
    }
    return hx::run_loop(std::move(config), sandbox, options.built_in_loop, finish_judge);
}

int report(const char* message, int status) noexcept {
    static_cast<void>(std::fputs("hx: ", stderr));
    static_cast<void>(std::fputs(message, stderr));
    static_cast<void>(std::fputc('\n', stderr));
    return status;
}

}  // namespace

int main(int argc, char** argv) {
    try {
        // NOLINTNEXTLINE(cppcoreguidelines-pro-bounds-pointer-arithmetic)
        return run(std::vector<std::string_view>(argv + std::min(argc, 1), argv + argc));
    } catch (const UsageError& error) {
        return report(error.what(), kExitUsage);
    } catch (const std::exception& error) {
        return report(error.what(), kExitFailure);
    } catch (...) {
        return report("unknown error", kExitFailure);
    }
}
