#include <dirent.h>
#include <fcntl.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <charconv>
#include <csignal>
#include <cstddef>
#include <cstring>
#include <ctime>
#include <span>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include "host.h"
#include "host_internal.h"
#include "sys.h"

namespace hx {

// NOLINTBEGIN(cppcoreguidelines-avoid-non-const-global-variables)
volatile std::sig_atomic_t g_signal = 0;
volatile std::sig_atomic_t g_expired = 1;
// NOLINTEND(cppcoreguidelines-avoid-non-const-global-variables)

namespace {

constexpr int kKillRounds = 100;
constexpr std::size_t kMaxKilled = 4096;

pid_t parent_of(int proc, std::string_view pid) {
    constexpr std::string_view kStat = "/stat";
    std::array<char, 64> path{};
    if (pid.size() + kStat.size() >= path.size()) {
        return 0;
    }
    std::memcpy(path.data(), pid.data(), pid.size());
    std::memcpy(path.data() + pid.size(), kStat.data(), kStat.size());
    const UniqueFd file(openat(proc, path.data(), O_RDONLY | O_CLOEXEC));
    std::array<char, 512> buffer{};
    const ssize_t n = file.valid() ? read(file.get(), buffer.data(), buffer.size()) : -1;
    if (n <= 0) {
        return 0;
    }
    const std::string_view stat(buffer.data(), static_cast<std::size_t>(n));
    const std::size_t close = stat.rfind(')');
    if (close == std::string_view::npos || stat.size() < close + 4) {
        return 0;
    }
    const std::string_view fields = stat.substr(close + 4);
    pid_t parent = 0;
    std::from_chars(fields.data(), fields.data() + fields.size(), parent);
    return parent;
}

template <typename Fn>
bool for_each_process(const Fn& fn) noexcept {
    const UniqueFd proc(open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    if (!proc.valid()) {
        return false;
    }
    alignas(dirent64) std::array<char, 4096> entries{};
    for (;;) {
        const ssize_t n = getdents64(proc.get(), entries.data(), entries.size());
        if (n <= 0) {
            return true;
        }
        std::size_t at = 0;
        while (at < static_cast<std::size_t>(n)) {
            unsigned short size = 0;
            std::memcpy(&size, entries.data() + at + offsetof(dirent64, d_reclen), sizeof(size));
            const std::string_view name(entries.data() + at + offsetof(dirent64, d_name));
            at += size;
            pid_t pid = 0;
            const auto [stop, error] = std::from_chars(name.data(), name.data() + name.size(), pid);
            if (error == std::errc() && stop == name.data() + name.size()) {
                fn(proc.get(), name, pid);
            }
        }
    }
}

void on_signal(int sig) {
    if (g_signal != 0) {
        kill_descendants();
        _exit(128 + sig);
    }
    g_signal = sig;
}

void on_alarm(int /*sig*/) {
    g_expired = 1;
}

}  // namespace

double now() noexcept {
    timespec ts{};
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return static_cast<double>(ts.tv_sec) + static_cast<double>(ts.tv_nsec) * 1e-9;
}

void wait_for(std::span<pollfd> fds, double seconds) noexcept {
    sigset_t stops;
    sigset_t before;
    sigemptyset(&stops);
    for (const int sig : {SIGINT, SIGTERM, SIGHUP}) {
        sigaddset(&stops, sig);
    }
    pthread_sigmask(SIG_BLOCK, &stops, &before);
    if (g_signal == 0) {
        seconds = std::min(seconds, kLongest);
        const auto whole = static_cast<time_t>(seconds);
        const timespec limit{whole, static_cast<long>((seconds - static_cast<double>(whole)) * 1e9)};
        ppoll(fds.data(), fds.size(), seconds < 0 ? nullptr : &limit, &before);
    }
    pthread_sigmask(SIG_SETMASK, &before, nullptr);
}

void kill_descendants(std::span<const pid_t> spared) noexcept {
    const pid_t self = getpid();
    std::array<pid_t, kMaxKilled> killed{};
    std::size_t count = 0;
    const auto was_killed = [&](pid_t pid) {
        return std::find(killed.begin(), killed.begin() + static_cast<std::ptrdiff_t>(count), pid) !=
               killed.begin() + static_cast<std::ptrdiff_t>(count);
    };
    for (int round = 0; round < kKillRounds; ++round) {
        bool found = false;
        const bool listed = for_each_process([&](int proc, std::string_view name, pid_t pid) {
            if (was_killed(pid) || std::binary_search(spared.begin(), spared.end(), pid)) {
                return;
            }
            const pid_t parent = parent_of(proc, name);
            if (parent != self && !was_killed(parent)) {
                return;
            }
            found = true;
            kill(pid, SIGKILL);
            if (count < killed.size()) {
                killed.at(count) = pid;
                ++count;
            }
        });
        if (!listed || !found) {
            break;
        }
    }
    const double deadline = now() + kReapSeconds;
    for (;;) {
        reap_exited();
        const bool left = std::any_of(killed.begin(), killed.begin() + static_cast<std::ptrdiff_t>(count),
                                      [](pid_t pid) { return kill(pid, 0) == 0; });
        if (!left || now() >= deadline) {
            return;
        }
        nap(10);
    }
}

std::vector<pid_t> descendants_now() {
    std::vector<std::pair<pid_t, pid_t>> parents;
    for_each_process(
        [&](int proc, std::string_view name, pid_t pid) { parents.emplace_back(pid, parent_of(proc, name)); });
    std::vector<pid_t> found{getpid()};
    for (std::size_t at = 0; at < found.size(); ++at) {
        for (const auto& [pid, parent] : parents) {
            if (parent == found[at]) {
                found.push_back(pid);
            }
        }
    }
    found.erase(found.begin());
    std::sort(found.begin(), found.end());
    return found;
}

void reap_exited() noexcept {
    while (waitpid(-1, nullptr, WNOHANG) > 0) {
    }
}

void install_signal_handlers() {
    struct sigaction action {};
    action.sa_handler = on_signal;
    sigemptyset(&action.sa_mask);
    action.sa_flags = 0;
    for (const int sig : {SIGINT, SIGTERM, SIGHUP}) {
        sigaction(sig, &action, nullptr);
    }
    action.sa_handler = on_alarm;
    action.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &action, nullptr);
    static_cast<void>(std::signal(SIGPIPE, SIG_IGN));
}

}  // namespace hx
