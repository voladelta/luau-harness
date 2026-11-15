#include "host.h"

#include <curl/curl.h>
#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cctype>
#include <cerrno>
#include <charconv>
#include <csignal>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <filesystem>
#include <initializer_list>
#include <iostream>
#include <memory>
#include <new>
#include <optional>
#include <span>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <fmt/format.h>

#include "lua.h"
#include "luacode.h"
#include "lualib.h"
#include "sys.h"

// The host functions raise Luau errors with C++ objects alive on the stack, which only an error thrown as a C++
// exception destroys; a longjmp would skip their destructors.
#if LUA_USE_LONGJMP
#error "hx needs Luau built to raise errors as C++ exceptions, not with longjmp"
#endif

namespace hx {
namespace {

constexpr std::size_t kMaxFile = std::size_t{64} << 20;       // host.read refuses larger files
constexpr std::size_t kMaxResponse = std::size_t{256} << 20;  // host.llm and host.tool abort a larger reply
constexpr std::size_t kMaxUpload = std::size_t{100} << 20;    // the largest file DeepSeek's OCR takes
constexpr std::size_t kCaptureHalf = std::size_t{512} << 10;  // host.sh keeps this much of each end of the output
constexpr std::size_t kBufferSize = 65536;
constexpr double kNoPidfdPoll = 0.05;     // seconds between looks at a command whose exit cannot be waited for
constexpr int kKillRounds = 100;          // at exit, so that a fork storm cannot hold hx up forever
constexpr std::size_t kMaxKilled = 4096;  // processes remembered at exit; past that, more rounds find the rest
constexpr double kReapSeconds = 1.0;      // how long hx waits for a process it killed to be gone
constexpr double kLongest = 1e9;          // seconds, about 30 years: the longest duration a call can ask for

// Signal handlers cannot be given context, so this state is global. It is the only mutable global state here.
// NOLINTBEGIN(cppcoreguidelines-avoid-non-const-global-variables)
volatile std::sig_atomic_t g_signal = 0;  // a stop in progress: the signal that asked for it
// The time limit's timer has fired, so the next VM safepoint reads the clock. It starts set and stays set until a
// timer exists: without one, every safepoint reads the clock, which is slower and no less right.
volatile std::sig_atomic_t g_expired = 1;
// NOLINTEND(cppcoreguidelines-avoid-non-const-global-variables)

double now() {
    timespec ts{};
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return static_cast<double>(ts.tv_sec) + static_cast<double>(ts.tv_nsec) * 1e-9;
}

// The conventional "nil, message" return for a failure the caller is expected to handle.
int fail(lua_State* L, std::string_view message) {
    lua_pushnil(L);
    lua_pushlstring(L, message.data(), message.size());
    return 2;
}

int fail_errno(lua_State* L, std::string_view path) {
    return fail(L, fmt::format("{}: {}", path, error_text()));
}

// A string argument that goes to the system as a C string. One with a NUL byte in it would be cut short there
// without a word, so it is refused.
const char* check_cstring(lua_State* L, int idx) {
    std::size_t length = 0;
    const char* text = luaL_checklstring(L, idx, &length);
    if (std::strlen(text) != length) {
        luaL_argerror(L, idx, "contains a NUL byte");
    }
    return text;
}

// Reads an optional numeric field of the options table at idx.
double option(lua_State* L, int idx, const char* name, double fallback) {
    if (lua_isnoneornil(L, idx)) {
        return fallback;
    }
    luaL_checktype(L, idx, LUA_TTABLE);
    lua_getfield(L, idx, name);
    const double value = lua_isnil(L, -1) ? fallback : luaL_checknumber(L, -1);
    lua_pop(L, 1);
    return value;
}

// The `beta` field of host.llm's options: the anthropic-beta header, a comma-separated list of beta names. Only
// letters, digits, '-', '_' and ',' are taken, so the value cannot end the header and start another.
std::string beta_option(lua_State* L, int idx) {
    if (lua_isnoneornil(L, idx)) {
        return {};
    }
    luaL_checktype(L, idx, LUA_TTABLE);
    lua_getfield(L, idx, "beta");
    std::string value;
    if (!lua_isnil(L, -1)) {
        std::size_t length = 0;
        const char* text = luaL_checklstring(L, -1, &length);
        value.assign(text, length);
    }
    lua_pop(L, 1);
    for (const char c : value) {
        if (std::isalnum(static_cast<unsigned char>(c)) == 0 && c != '-' && c != '_' && c != ',') {
            luaL_error(L, "host.llm: beta is beta names separated by commas, not %s", value.c_str());
        }
    }
    return value;
}

// A duration from Luau in seconds. A negative one (or nan) is empty, and a huge one is cut to kLongest, so every
// conversion to milliseconds or nanoseconds stays defined.
double duration(double seconds) {
    return seconds > 0 ? std::min(seconds, kLongest) : 0.0;
}

const char* stop_error(int sig) {
    return sig == SIGINT ? "interrupted" : "terminated";
}

// Raises the stop in progress as its Luau error. The stop is not over: every later safepoint and blocking call
// raises it again, until the outermost host.try or host.timed returns, so no pcall can swallow it.
void raise_if_signaled(lua_State* L) {
    if (g_signal == 0) {
        return;
    }
    lua_pushstring(L, stop_error(g_signal));
    lua_error(L);
}

// Counts a host.try or host.timed call for as long as it runs.
class TryScope {
  public:
    explicit TryScope(int& depth) noexcept : depth_(depth) { ++depth_; }
    TryScope(const TryScope&) = delete;
    TryScope& operator=(const TryScope&) = delete;
    ~TryScope() { --depth_; }

  private:
    int& depth_;
};

// The process's one timer, which raises SIGALRM at the time limit in force, or null when the kernel gave none.
timer_t* limit_timer() noexcept {
    static timer_t timer{};
    static const bool made = [] {
        sigevent event{};
        event.sigev_notify = SIGEV_SIGNAL;
        event.sigev_signo = SIGALRM;
        return timer_create(CLOCK_MONOTONIC, &event, &timer) == 0;
    }();
    return made ? &timer : nullptr;
}

// Sets the timer to fire at deadline, a time on the monotonic clock; 0 turns it off. A deadline already past fires
// at once. The VM is interrupted by a flag the signal sets, so a tight loop pays for no clock read, and the limit is
// as exact as the next safepoint, however slow the steps between safepoints are.
void arm_limit(double deadline) noexcept {
    timer_t* timer = limit_timer();
    if (timer == nullptr) {
        return;  // g_expired stays set, and every safepoint reads the clock
    }
    const auto whole = static_cast<time_t>(deadline);
    const itimerspec at{{0, 0}, {whole, static_cast<long>((deadline - static_cast<double>(whole)) * 1e9)}};
    timer_settime(*timer, TIMER_ABSTIME, &at, nullptr);
}

// Tightens a time limit for as long as the scope lives, then puts the outer limit back. A limit of 0 means none.
class DeadlineScope {
  public:
    DeadlineScope(double& slot, double limit) noexcept : slot_(slot), outer_(slot) {
        slot_ = limit <= 0 || (outer_ > 0 && outer_ < limit) ? outer_ : limit;
        arm_limit(slot_);
    }
    DeadlineScope(const DeadlineScope&) = delete;
    DeadlineScope& operator=(const DeadlineScope&) = delete;
    ~DeadlineScope() {
        slot_ = outer_;
        arm_limit(slot_);
    }

  private:
    double& slot_;
    double outer_;
};

// --- processes --------------------------------------------------------------------------------------------
//
// The signal handler leaves through kill_descendants, so everything here is async-signal-safe: system calls and
// fixed buffers only, nothing that allocates or takes a lock.

// The parent of a process, read from /proc/PID/stat: "PID (COMM) STATE PPID ...". COMM can hold spaces and
// parentheses, so the fields are counted from the last ')'. Returns 0 when that cannot be read.
pid_t parent_of(int proc, std::string_view pid) {
    constexpr std::string_view kStat = "/stat";
    std::array<char, 64> path{};  // "PID/stat", relative to /proc
    if (pid.size() + kStat.size() >= path.size()) {
        return 0;
    }
    std::memcpy(path.data(), pid.data(), pid.size());
    std::memcpy(path.data() + pid.size(), kStat.data(), kStat.size());
    const UniqueFd file(openat(proc, path.data(), O_RDONLY | O_CLOEXEC));
    std::array<char, 512> buffer{};  // reaches well past PPID: COMM is at most 64 bytes
    const ssize_t n = file.valid() ? read(file.get(), buffer.data(), buffer.size()) : -1;
    if (n <= 0) {
        return 0;
    }
    const std::string_view stat(buffer.data(), static_cast<std::size_t>(n));
    const std::size_t close = stat.rfind(')');
    if (close == std::string_view::npos || stat.size() < close + 4) {
        return 0;
    }
    const std::string_view fields = stat.substr(close + 4);  // past ") S "
    pid_t parent = 0;
    std::from_chars(fields.data(), fields.data() + fields.size(), parent);
    return parent;
}

// Calls fn(proc, name, pid) for each process in /proc, where proc is /proc open and name the PID as text. Returns
// false when /proc cannot be read.
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

// Kills every descendant of hx that is not spared (a sorted list of PIDs), then waits a moment for them to be gone.
// A descendant is a child of hx or of a process killed here, so the whole tree is killed without waiting for any
// part of it to die: SIGKILL cannot end a process stuck in uninterruptible I/O (a dead NFS server) until the I/O
// returns, and hx does not wait on one longer than kReapSeconds. Nothing is reaped until every process is killed,
// so the zombies hold their PIDs and none can be reused by a process that is not a descendant.
void kill_descendants(std::span<const pid_t> spared = {}) noexcept {
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
            // Once kill returns, a fork in that process either has finished, so its child is listed by a later
            // scan, or fails.
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
    // A killed process whose children were killed too passes them to hx as it dies, so reaping every child that
    // has exited, until no killed process is left, reaps the whole tree.
    const double deadline = now() + kReapSeconds;
    for (;;) {
        while (waitpid(-1, nullptr, WNOHANG) > 0) {
        }
        const bool left = std::any_of(killed.begin(), killed.begin() + static_cast<std::ptrdiff_t>(count),
                                      [](pid_t pid) { return kill(pid, 0) == 0; });
        if (!left || now() >= deadline) {
            return;
        }
        const timespec nap{0, 10'000'000};
        nanosleep(&nap, nullptr);
    }
}

// The descendants of hx now, sorted: what kill_descendants spares after a command behind a narrower wall.
std::vector<pid_t> descendants_now() {
    std::vector<std::pair<pid_t, pid_t>> parents;  // pid, parent
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

void on_signal(int sig) {
    if (g_signal != 0) {
        // The first stop has not reached the end of its scope yet, so the loop is stuck: leave now. No destructor
        // runs on the way out, so what the model started is killed here.
        kill_descendants();
        _exit(128 + sig);
    }
    g_signal = sig;
}

void on_alarm(int /*sig*/) {
    g_expired = 1;
}

// Waits until one of fds is ready, for at most seconds (negative: without limit), or until a signal arrives. A stop
// that arrives just before the wait is not slept through: the stop signals are held back while g_signal is looked
// at, and ppoll lets them in again as it starts to wait.
void wait_for(std::span<pollfd> fds, double seconds) noexcept {
    sigset_t stops;
    sigset_t before;
    sigemptyset(&stops);
    for (const int sig : {SIGINT, SIGTERM, SIGHUP}) {
        sigaddset(&stops, sig);
    }
    sigprocmask(SIG_BLOCK, &stops, &before);
    if (g_signal == 0) {
        seconds = std::min(seconds, kLongest);
        const auto whole = static_cast<time_t>(seconds);
        const timespec limit{whole, static_cast<long>((seconds - static_cast<double>(whole)) * 1e9)};
        ppoll(fds.data(), fds.size(), seconds < 0 ? nullptr : &limit, &before);
    }
    sigprocmask(SIG_SETMASK, &before, nullptr);
}

// --- host.llm ---------------------------------------------------------------------------------------------

std::size_t on_body(char* data, std::size_t size, std::size_t count, void* user) noexcept {
    auto* body = static_cast<std::string*>(user);
    const std::size_t bytes = size * count;
    if (body->size() + bytes > kMaxResponse) {
        return 0;  // a short count aborts the transfer
    }
    try {
        body->append(data, bytes);
    } catch (...) {
        return 0;
    }
    return bytes;
}

int on_progress(void* /*user*/, curl_off_t /*dltotal*/, curl_off_t /*dlnow*/, curl_off_t /*ultotal*/,
                curl_off_t /*ulnow*/) noexcept {
    return g_signal != 0 ? 1 : 0;  // non-zero aborts the transfer
}

// An owned list of request headers.
class Headers {
  public:
    Headers() = default;
    Headers(const Headers&) = delete;
    Headers& operator=(const Headers&) = delete;
    ~Headers() { curl_slist_free_all(list_); }

    void add(const std::string& header) {
        curl_slist* grown = curl_slist_append(list_, header.c_str());
        if (grown == nullptr) {
            throw std::bad_alloc();
        }
        list_ = grown;
    }

    [[nodiscard]] curl_slist* get() const noexcept { return list_; }

  private:
    curl_slist* list_ = nullptr;
};

struct CurlMimeDeleter {
    void operator()(curl_mime* mime) const noexcept { curl_mime_free(mime); }
};
using CurlMime = std::unique_ptr<curl_mime, CurlMimeDeleter>;

// Sends a POST whose body is already set on curl to endpoint, with its key, its headers and `extra`, and returns
// status, text, and the seconds of a Retry-After header if there was one | nil, error. Redirects are not followed: a
// key goes to the URL it was meant for and nowhere else. The handle is reset afterwards, so it keeps no pointer into
// this request; the connection stays open for the next.
int post(lua_State* L, CURL* curl, const Endpoint& endpoint, std::initializer_list<std::string_view> extra,
         double timeout, const std::string& ca_bundle) {
    Headers headers;
    if (!endpoint.key.empty()) {
        headers.add(endpoint.auth == Auth::Bearer ? fmt::format("Authorization: Bearer {}", endpoint.key)
                                                  : fmt::format("x-api-key: {}", endpoint.key));
    }
    for (const std::string& header : endpoint.headers) {
        headers.add(header);
    }
    for (const std::string_view header : extra) {
        if (!header.empty()) {
            headers.add(std::string(header));
        }
    }
    std::string response;
    std::array<char, CURL_ERROR_SIZE> error_buffer{};
    curl_easy_setopt(curl, CURLOPT_URL, endpoint.url.c_str());
    curl_easy_setopt(curl, CURLOPT_PROTOCOLS_STR, "http,https");
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers.get());
    // A model can take minutes to answer a non-streaming request, with nothing on the wire meanwhile; probes keep
    // a NAT or a proxy on the way from dropping the connection as idle.
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPALIVE, 1L);
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPIDLE, 60L);
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPINTVL, 30L);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, on_body);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA, &response);
    curl_easy_setopt(curl, CURLOPT_XFERINFOFUNCTION, on_progress);
    curl_easy_setopt(curl, CURLOPT_NOPROGRESS, 0L);
    curl_easy_setopt(curl, CURLOPT_NOSIGNAL, 1L);
    curl_easy_setopt(curl, CURLOPT_CONNECTTIMEOUT, 30L);
    // Zero would mean "no limit" to libcurl, so an exhausted limit becomes the shortest one.
    curl_easy_setopt(curl, CURLOPT_TIMEOUT_MS, std::max(1L, static_cast<long>(timeout * 1000)));
    curl_easy_setopt(curl, CURLOPT_ERRORBUFFER, error_buffer.data());
    if (!ca_bundle.empty()) {
        curl_easy_setopt(curl, CURLOPT_CAINFO, ca_bundle.c_str());
    }

    const CURLcode code = curl_easy_perform(curl);
    long status = 0;
    curl_off_t retry_after = 0;  // seconds the server asked for in Retry-After; 0 when it sent none
    curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &status);
    curl_easy_getinfo(curl, CURLINFO_RETRY_AFTER, &retry_after);
    curl_easy_reset(curl);
    if (code != CURLE_OK) {
        raise_if_signaled(L);
        return fail(L, error_buffer.front() != '\0' ? error_buffer.data() : curl_easy_strerror(code));
    }
    lua_pushnumber(L, static_cast<double>(status));
    lua_pushlstring(L, response.data(), response.size());
    if (retry_after <= 0) {
        return 2;
    }
    lua_pushnumber(L, static_cast<double>(retry_after));
    return 3;
}

// --- host.sh ----------------------------------------------------------------------------------------------

// Keeps the first and the last part of a stream and counts what fell out of the middle: the start of a command's
// output says what ran, the end says how it failed.
class Capture {
  public:
    void add(std::string_view data) {
        if (head_.size() < kCaptureHalf) {
            const std::size_t take = std::min(data.size(), kCaptureHalf - head_.size());
            head_.append(data.substr(0, take));
            data.remove_prefix(take);
        }
        if (data.empty()) {
            return;
        }
        tail_.append(data);
        if (tail_.size() > 2 * kCaptureHalf) {
            const std::size_t cut = tail_.size() - kCaptureHalf;
            dropped_ += cut;
            tail_.erase(0, cut);
        }
    }

    // At most kCaptureHalf from each end. Each part ends or starts at a line where one is near, and never inside
    // a character: half a character is not valid UTF-8, and would be lost without a word.
    [[nodiscard]] std::string text() const {
        if (head_.size() + tail_.size() <= 2 * kCaptureHalf) {
            return head_ + tail_;  // nothing was dropped: the tail only fills once the head is full
        }
        const auto continues = [](char c) { return (static_cast<unsigned char>(c) & 0xC0U) == 0x80U; };
        std::string_view head = head_;
        std::string_view tail = std::string_view(tail_).substr(tail_.size() - kCaptureHalf);
        if (const std::size_t line = head.rfind('\n'); line != std::string_view::npos && line > head.size() / 2) {
            head = head.substr(0, line + 1);
        } else {
            while (!head.empty() && continues(head.back())) {
                head.remove_suffix(1);
            }
            if (!head.empty() && static_cast<unsigned char>(head.back()) >= 0xC0U) {
                head.remove_suffix(1);  // the lead byte of the character that was cut
            }
        }
        if (const std::size_t line = tail.find('\n'); line != std::string_view::npos && line < tail.size() / 2) {
            tail.remove_prefix(line + 1);
        } else {
            while (!tail.empty() && continues(tail.front())) {
                tail.remove_prefix(1);
            }
        }
        const std::size_t omitted = dropped_ + head_.size() + tail_.size() - head.size() - tail.size();
        return fmt::format("{}{}[... {} bytes of output omitted ...]\n{}", head, head.ends_with('\n') ? "" : "\n",
                           omitted, tail);
    }

  private:
    std::string head_;
    std::string tail_;
    std::size_t dropped_ = 0;
};

void kill_group(pid_t group) {
    // Until the child has called setsid() its group does not exist yet; then the child itself is the target.
    if (kill(-group, SIGKILL) != 0) {
        kill(group, SIGKILL);
    }
}

// Waits for a killed child for at most seconds. SIGKILL cannot end a process stuck in uninterruptible I/O (a dead
// network mount) until the I/O returns, so it may take longer; it is then left a zombie for a later reap, by the
// next host.sh or by kill_descendants.
bool reap_within(pid_t pid, int& status, double seconds) {
    const double deadline = now() + seconds;
    for (;;) {
        const pid_t done = waitpid(pid, &status, WNOHANG);
        if (done == pid) {
            return true;
        }
        if ((done < 0 && errno != EINTR) || now() >= deadline) {
            return false;
        }
        const timespec nap{0, 10'000'000};
        nanosleep(&nap, nullptr);
    }
}

// Runs in the forked child, so it may only call async-signal-safe functions: no allocation, no stdio.
[[noreturn]] void exec_bash(const char* command, int output, const Wall* wall) {
    // A session of its own: one process group to kill, and no terminal to ask a question on.
    setsid();
    if (wall != nullptr && !wall->enter()) {
        constexpr std::string_view message = "hx: cannot put up the command's wall\n";
        if (write(output, message.data(), message.size()) < 0) {
            // Nothing more can be said.
        }
        _exit(126);
    }
    const int null = open("/dev/null", O_RDONLY);
    if (null > STDIN_FILENO) {
        dup2(null, STDIN_FILENO);
        close(null);  // only the copy on stdin is for the command
    }
    dup2(output, STDOUT_FILENO);
    dup2(output, STDERR_FILENO);
    // Nothing else hx holds is for the command: libcurl keeps the connection it reuses, and its wake-up pipe, open
    // between requests, and does not make them close-on-exec. If this fails, the exec still goes ahead.
#ifdef CLOSE_RANGE_CLOEXEC  // glibc 2.34 declares the wrapper and the flag together
    close_range(3, ~0U, CLOSE_RANGE_CLOEXEC);
#else  // the system call is Linux 5.11, the same number on every architecture; main checks it works
    syscall(436, 3U, ~0U, 4U);
#endif
    for (const int sig : {SIGINT, SIGTERM, SIGHUP, SIGPIPE}) {
        static_cast<void>(std::signal(sig, SIG_DFL));
    }
    execlp("bash", "bash", "-c", command, static_cast<char*>(nullptr));
    constexpr std::string_view message = "hx: cannot run bash\n";
    if (write(STDERR_FILENO, message.data(), message.size()) < 0) {
        // Nothing more can be said.
    }
    _exit(127);
}

// --- files ------------------------------------------------------------------------------------------------

int l_read(lua_State* L) {
    const char* path = check_cstring(L, 1);
    const UniqueFd file(open(path, O_RDONLY | O_CLOEXEC));
    if (!file.valid()) {
        return fail_errno(L, path);
    }
    struct stat st {};
    if (fstat(file.get(), &st) != 0) {
        return fail_errno(L, path);
    }
    if (S_ISDIR(st.st_mode)) {
        return fail(L, fmt::format("{}: is a directory", path));
    }

    std::string data;
    std::array<char, kBufferSize> buffer{};
    for (;;) {
        const ssize_t n = read(file.get(), buffer.data(), buffer.size());
        if (n == 0) {
            break;
        }
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return fail_errno(L, path);
        }
        if (data.size() + static_cast<std::size_t>(n) > kMaxFile) {
            return fail(L, fmt::format("{}: larger than 64 MiB", path));
        }
        data.append(buffer.data(), static_cast<std::size_t>(n));
    }
    lua_pushlstring(L, data.data(), data.size());
    return 1;
}

// Writes all of data, going on after a signal. Returns false with errno set when the system refuses.
bool write_all(int fd, std::string_view data) {
    while (!data.empty()) {
        const ssize_t n = write(fd, data.data(), data.size());
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return false;
        }
        data.remove_prefix(static_cast<std::size_t>(n));
    }
    return true;
}

// The permission bits a file created now gets: 0666 less the umask, which can only be read by setting it.
mode_t new_file_mode() {
    const mode_t mask = umask(0);
    umask(mask);
    return 0666 & ~mask;
}

// Writes data to a new file beside target, gives it `mode` and renames it over target. False, with errno set, when
// the write or the rename failed: target is then as it was. `unable` is also set when no new file could be made
// (a directory that refuses one), so nothing was tried.
bool replace_file(const std::filesystem::path& target, mode_t mode, std::string_view data, bool& unable) {
    std::string temp = (target.parent_path() / fmt::format(".{}.hx-XXXXXX", target.filename().string())).string();
    const UniqueFd file(mkostemp(temp.data(), O_CLOEXEC));
    unable = !file.valid() || fchmod(file.get(), mode) != 0;
    if (unable || !write_all(file.get(), data) || rename(temp.c_str(), target.c_str()) != 0) {
        const int error = errno;
        if (file.valid()) {
            unlink(temp.c_str());
        }
        errno = error;
        return false;
    }
    return true;
}

// A regular file is written by writing a new file beside it and renaming that over it, so a full disk or a failed
// write leaves the old content or the new, never a file cut short; a hx killed mid-write can leave the new file
// behind. Nothing is synced: a power loss can still lose both, and that is not what this guards against. The new
// file gets the old one's mode, not its owner or extended attributes: git is the way back. A link is followed, so
// the file it names is replaced and the link stays. What a new file cannot stand in for is written in place:
// something other than a regular file (/dev/null, a FIFO), a file with other names that must go on seeing it, a
// dangling link, a file in a directory that refuses a new file (a lone --rw file), and a file that may not be
// written (a rename would not ask it, so the open is left to refuse).
int l_write(lua_State* L) {
    const char* path = check_cstring(L, 1);
    std::size_t length = 0;
    const char* raw = luaL_checklstring(L, 2, &length);
    const std::string_view data(raw, length);

    std::error_code ec;
    const std::filesystem::path parent = std::filesystem::path(path).parent_path();
    if (!parent.empty()) {
        std::filesystem::create_directories(parent, ec);
        if (ec) {
            return fail(L, fmt::format("{}: {}", parent.string(), ec.message()));
        }
    }

    struct stat st {};
    struct stat entry {};
    const bool exists = stat(path, &st) == 0;
    const bool missing = !exists && errno == ENOENT && lstat(path, &entry) != 0;  // not even a dangling link
    if ((exists && S_ISREG(st.st_mode) && st.st_nlink == 1 && access(path, W_OK) == 0) || missing) {
        const std::filesystem::path target = exists ? std::filesystem::canonical(path, ec) : path;
        bool unable = false;
        if (!ec && replace_file(target, exists ? st.st_mode & 07777 : new_file_mode(), data, unable)) {
            lua_pushboolean(L, 1);
            return 1;
        }
        if (!ec && !unable) {
            return fail_errno(L, path);
        }
    }

    const UniqueFd file(open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0666));
    if (!file.valid() || !write_all(file.get(), data)) {
        return fail_errno(L, path);
    }
    lua_pushboolean(L, 1);
    return 1;
}

int l_list(lua_State* L) {
    const char* path = check_cstring(L, 1);
    std::error_code ec;
    std::vector<std::string> names;
    for (std::filesystem::directory_iterator it(path, ec), end; !ec && it != end; it.increment(ec)) {
        std::string name = it->path().filename().string();
        std::error_code type_ec;
        if (it->is_directory(type_ec)) {
            name.push_back('/');
        }
        names.push_back(std::move(name));
    }
    if (ec) {
        return fail(L, fmt::format("{}: {}", path, ec.message()));
    }
    std::sort(names.begin(), names.end());
    lua_createtable(L, static_cast<int>(names.size()), 0);
    int index = 0;
    for (const std::string& name : names) {
        lua_pushlstring(L, name.data(), name.size());
        lua_rawseti(L, -2, ++index);
    }
    return 1;
}

// --- the VM itself ----------------------------------------------------------------------------------------

int l_load(lua_State* L) {
    std::size_t length = 0;
    const char* source = luaL_checklstring(L, 1, &length);
    const char* name = luaL_optstring(L, 2, "=chunk");
    const bool has_env = !lua_isnoneornil(L, 3);
    if (has_env) {
        luaL_checktype(L, 3, LUA_TTABLE);
    }

    std::size_t bytecode_size = 0;
    const std::unique_ptr<char, FreeDeleter> bytecode(luau_compile(source, length, nullptr, &bytecode_size));
    if (!bytecode) {
        luaL_error(L, "out of memory compiling %s", name);
    }
    // A syntax error travels inside the bytecode and comes out of luau_load as its message.
    if (luau_load(L, name, bytecode.get(), bytecode_size, 0) != 0) {
        lua_pushnil(L);
        lua_insert(L, -2);
        return 2;
    }
    if (has_env) {
        lua_pushvalue(L, 3);
        lua_setfenv(L, -2);
    }
    return 1;
}

// --- small things -----------------------------------------------------------------------------------------

int l_input(lua_State* L) {
    raise_if_signaled(L);
    const char* prompt = luaL_optstring(L, 1, "");
    fmt::print(stderr, "{}", prompt);
    static_cast<void>(std::fflush(stderr));
    std::string line;
    if (!std::getline(std::cin, line)) {
        if (g_signal != 0) {
            // The read was cut short, not ended. Left as it is, the stream would report the end of input to
            // every later call, and a loop that outlives the stop could never read again.
            std::cin.clear();
            std::clearerr(stdin);
            raise_if_signaled(L);
        }
        return 0;
    }
    lua_pushlstring(L, line.data(), line.size());
    return 1;
}

int l_log(lua_State* L) {
    std::size_t length = 0;
    const char* text = luaL_checklstring(L, 1, &length);
    fmt::print(stderr, "{}\n", std::string_view(text, length));
    return 0;
}

int l_env(lua_State* L) {
    const char* value = std::getenv(check_cstring(L, 1));  // NOLINT(concurrency-mt-unsafe): single-threaded
    if (value == nullptr) {
        return 0;
    }
    lua_pushstring(L, value);
    return 1;
}

}  // namespace

Host::Host(HostConfig config, const Sandbox& sandbox) : config_(std::move(config)), sandboxed_(sandbox.confined()) {
    // An orphan comes to hx instead of init, whatever session it moved to, so every process a command started stays
    // a descendant of hx and ~Host can find it.
    prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
}

Host::~Host() {
    kill_descendants();
}

Host& Host::of(lua_State* L) {
    return *static_cast<Host*>(lua_callbacks(L)->userdata);
}

// Runs at VM safepoints (loop back edges, calls). Luau code cannot block these, so a signal or an expired time
// limit always reaches the code that is running. Both are flags a signal handler sets, so a safepoint costs two
// loads; the clock is read only once the limit's timer has fired (see arm_limit).
void Host::on_interrupt(lua_State* L, int gc) {
    if (gc >= 0) {
        return;  // inside the collector: raising here is not allowed
    }
    raise_if_signaled(L);
    if (g_expired == 0) {
        return;
    }
    // Cleared before the clock is read, so a timer that fires meanwhile is not lost. It may have fired for a limit
    // that has ended since; then the clock says so and nothing is raised.
    g_expired = limit_timer() != nullptr ? 0 : 1;
    const Host& host = of(L);
    if (host.deadline_ > 0 && now() >= host.deadline_) {
        g_expired = 1;  // raised again at every safepoint until the limit's call returns, so no pcall swallows it
        lua_pushstring(L, "timeout");
        lua_error(L);
    }
}

const Endpoint* Host::endpoint(std::string_view name) const {
    const auto found = std::ranges::find(config_.endpoints, name, &Endpoint::name);
    return found == config_.endpoints.end() ? nullptr : &*found;
}

void Host::CurlDeleter::operator()(CURL* handle) const noexcept {
    curl_easy_cleanup(handle);
}

CURL* Host::curl() {
    if (!curl_) {
        curl_.reset(curl_easy_init());
    } else {
        curl_easy_reset(curl_.get());  // post resets it too, but a request that failed before reaching post may not
    }
    return curl_.get();
}

int Host::llm(lua_State* L) {
    std::size_t length = 0;
    const char* body = luaL_checklstring(L, 1, &length);
    Host& host = of(L);
    const double timeout = host.within_limit(option(L, 2, "timeout", 600));
    raise_if_signaled(L);  // a stop in progress starts no request
    const std::string beta = beta_option(L, 2);
    const Endpoint* endpoint = host.endpoint("llm");
    CURL* curl = host.curl();
    if (endpoint == nullptr || curl == nullptr) {
        return fail(L, "cannot start an HTTP request");
    }
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDSIZE_LARGE, static_cast<curl_off_t>(length));
    return post(L, curl, *endpoint,
                {"Content-Type: application/json", beta.empty() ? "" : fmt::format("anthropic-beta: {}", beta)},
                timeout, host.config_.ca_bundle);
}

// host.tool(name, request, {timeout=300}): a request to the endpoint of that name, other than host.llm's. Only the
// endpoints' names are taken, so each key reaches its own URL and nothing else. A string request is sent as JSON;
// for an endpoint that takes a form, a table is sent as a multipart form, in which `file` names a file to upload
// and every other field is a text field.
int Host::tool(lua_State* L) {
    const std::string_view name = check_cstring(L, 1);
    Host& host = of(L);
    const Endpoint* endpoint = name == "llm" ? nullptr : host.endpoint(name);
    if (endpoint == nullptr) {
        std::vector<std::string_view> names;
        for (const Endpoint& each : host.config_.endpoints) {
            if (each.name != "llm") {
                names.push_back(each.name);
            }
        }
        std::string known;  // "a, b and c"
        for (std::size_t i = 0; i < names.size(); ++i) {
            if (i > 0) {
                known += i + 1 == names.size() ? " and " : ", ";
            }
            known += names[i];
        }
        luaL_error(L, "host.tool: no tool %s; the tools are %s", std::string(name).c_str(), known.c_str());
    }
    const bool form = endpoint->form && lua_istable(L, 2);
    if (!form) {
        luaL_checktype(L, 2, LUA_TSTRING);
    }
    const double timeout = host.within_limit(option(L, 3, "timeout", 300));
    raise_if_signaled(L);
    if (endpoint->key.empty() && !endpoint->key_env.empty()) {
        return fail(L, fmt::format("no {}: hx was started without one, so host.tool(\"{}\") cannot be used",
                                   endpoint->key_env, endpoint->name));
    }

    CURL* curl = host.curl();
    if (curl == nullptr) {
        return fail(L, "cannot start an HTTP request");
    }
    const CurlMime mime(form ? curl_mime_init(curl) : nullptr);
    if (form) {
        if (!mime) {
            return fail(L, "cannot start an HTTP request");
        }
        // Fields go in sorted order, so a request is the same whatever order the table was built in.
        std::vector<std::pair<std::string, std::string>> fields;
        lua_pushnil(L);
        while (lua_next(L, 2) != 0) {
            if (lua_type(L, -2) != LUA_TSTRING || lua_type(L, -1) != LUA_TSTRING) {
                luaL_error(L, "host.tool: a form is a table of strings by name");
            }
            std::size_t key_length = 0;
            std::size_t value_length = 0;
            const char* key = lua_tolstring(L, -2, &key_length);
            const char* value = lua_tolstring(L, -1, &value_length);
            fields.emplace_back(std::string(key, key_length), std::string(value, value_length));
            lua_pop(L, 1);
        }
        std::ranges::sort(fields);
        for (const auto& [key, value] : fields) {
            curl_mimepart* part = curl_mime_addpart(mime.get());
            curl_mime_name(part, key.c_str());
            if (key != "file") {
                curl_mime_data(part, value.data(), value.size());
                continue;
            }
            // Checked here, so a file that cannot be sent is reported as such rather than as a failed transfer.
            if (value.find('\0') != std::string::npos) {
                return fail(L, "host.tool: the file name contains a NUL byte");
            }
            const UniqueFd file(::open(value.c_str(), O_RDONLY | O_CLOEXEC));
            struct stat st {};
            if (!file.valid() || fstat(file.get(), &st) != 0) {
                return fail_errno(L, value);
            }
            if (!S_ISREG(st.st_mode)) {
                return fail(L, fmt::format("{}: not a regular file", value));
            }
            if (static_cast<std::size_t>(st.st_size) > kMaxUpload) {
                return fail(L, fmt::format("{}: larger than 100 MiB", value));
            }
            if (curl_mime_filedata(part, value.c_str()) != CURLE_OK) {
                return fail(L, fmt::format("{}: cannot be attached", value));
            }
        }
        curl_easy_setopt(curl, CURLOPT_MIMEPOST, mime.get());
        return post(L, curl, *endpoint, {}, timeout, host.config_.ca_bundle);
    }
    std::size_t length = 0;
    const char* body = lua_tolstring(L, 2, &length);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDSIZE_LARGE, static_cast<curl_off_t>(length));
    return post(L, curl, *endpoint, {"Content-Type: application/json"}, timeout, host.config_.ca_bundle);
}

int Host::sh(lua_State* L) {
    const char* command = check_cstring(L, 1);
    Host& host = of(L);
    const double timeout = host.within_limit(option(L, 2, "timeout", 120));
    const std::optional<Wall> wall = host.narrower_wall(L, 2);
    bool on_hangup = false;  // {hangup = true}: the command ends when stdin's writer hangs up
    if (lua_istable(L, 2)) {
        lua_getfield(L, 2, "hangup");
        on_hangup = lua_toboolean(L, -1) != 0;
        lua_pop(L, 1);
    }
    // A command behind a narrower wall is not trusted to leave anything running: what it started dies with it.
    const std::vector<pid_t> before = wall ? descendants_now() : std::vector<pid_t>{};
    raise_if_signaled(L);  // a stop in progress starts no command
    if (on_hangup) {
        pollfd already{STDIN_FILENO, POLLRDHUP, 0};
        if (poll(&already, 1, 0) > 0 && (already.revents & (POLLRDHUP | POLLHUP)) != 0) {
            lua_pushstring(L, "hung up");
            lua_error(L);
        }
    }
    // Adopted orphans that have exited are zombies of hx. No command of hx's own is running now, so this cannot
    // take the status of one; a judge that has exited is reaped here too, which finish_judge allows for.
    while (waitpid(-1, nullptr, WNOHANG) > 0) {
    }

    std::array<int, 2> pipe_fds{};
    if (pipe2(pipe_fds.data(), O_CLOEXEC) != 0) {
        luaL_error(L, "pipe: %s", error_text().c_str());
    }
    UniqueFd output(pipe_fds[0]);
    UniqueFd input(pipe_fds[1]);
    const double started = now();
    const pid_t pid = fork();
    if (pid < 0) {
        luaL_error(L, "fork: %s", error_text().c_str());
    }
    if (pid == 0) {
        exec_bash(command, input.get(), wall ? &*wall : nullptr);
    }
    input.reset();
    // Readable when bash exits, so its end is seen at once, not at the next poll. Linux 5.3; without it, polling.
    const UniqueFd exited(static_cast<int>(syscall(SYS_pidfd_open, pid, 0)));
    fcntl(output.get(), F_SETFL, O_NONBLOCK);

    Capture capture;
    std::array<char, kBufferSize> buffer{};
    bool at_end = false;  // every writer has closed the pipe
    const auto drain = [&] {
        while (!at_end) {
            const ssize_t n = read(output.get(), buffer.data(), buffer.size());
            if (n > 0) {
                capture.add({buffer.data(), static_cast<std::size_t>(n)});
            } else if (n == 0) {
                at_end = true;
            } else if (errno != EINTR) {
                break;  // EAGAIN: nothing more for now
            }
        }
    };

    // The command is Running until it exits by itself, runs out of time, or a signal arrives. The last two
    // kill its whole process group; a signal then leaves through raise_if_signaled.
    enum class Ending : std::uint8_t { Exited, TimedOut, Signaled, HungUp };
    const int hangup = on_hangup ? STDIN_FILENO : -1;
    const double deadline = now() + timeout;
    int status = 0;
    bool reaped = true;  // false: killed, but stuck in uninterruptible I/O and not yet dead
    Ending ending = Ending::Exited;
    double ended = 0;
    for (;;) {
        drain();
        if (waitpid(pid, &status, WNOHANG) == pid) {
            ended = now();
            break;
        }
        if (g_signal != 0 || now() >= deadline) {
            ending = g_signal != 0 ? Ending::Signaled : Ending::TimedOut;
            kill_group(pid);
            reaped = reap_within(pid, status, kReapSeconds);
            break;
        }
        // Wake on bash's exit, a hang-up, output, a signal or the deadline. A negative descriptor is not watched.
        std::array<pollfd, 3> waiters{{{exited.get(), POLLIN, 0}, {hangup, POLLRDHUP, 0}, {output.get(), POLLIN, 0}}};
        const double left = std::max(deadline - now(), 0.0);
        wait_for(std::span(waiters).first(at_end ? 2 : 3), exited.valid() ? left : std::min(left, kNoPidfdPoll));
        if ((waiters[1].revents & (POLLRDHUP | POLLHUP)) != 0) {
            ending = Ending::HungUp;
            kill_group(pid);
            reaped = reap_within(pid, status, kReapSeconds);
            break;
        }
    }
    if (ended == 0) {
        ended = now();
    }
    // bash is gone, or killed and left for a later reap. Whatever it left running keeps running, so do not wait
    // for the pipe to close; unless it ran behind a narrower wall, and then what it left is killed now.
    if (wall) {
        kill_descendants(before);
    }
    drain();
    if (ending == Ending::Signaled) {
        raise_if_signaled(L);
    }
    if (ending == Ending::HungUp) {
        lua_pushstring(L, "hung up");
        lua_error(L);
    }

    const std::string text = capture.text();
    lua_createtable(L, 0, 5);
    lua_pushlstring(L, text.data(), text.size());
    lua_setfield(L, -2, "out");
    if (!reaped) {
        lua_pushnumber(L, SIGKILL);  // what it was sent and will die of once its I/O returns
        lua_setfield(L, -2, "signal");
    } else if (WIFEXITED(status)) {
        lua_pushnumber(L, WEXITSTATUS(status));
        lua_setfield(L, -2, "code");
    } else if (WIFSIGNALED(status)) {
        lua_pushnumber(L, WTERMSIG(status));
        lua_setfield(L, -2, "signal");
    }
    lua_pushboolean(L, ending == Ending::TimedOut ? 1 : 0);
    lua_setfield(L, -2, "timed_out");
    lua_pushnumber(L, ended - started);
    lua_setfield(L, -2, "seconds");
    return 1;
}

// The wall that host.sh's options read and write ask for, or none when they ask for nothing.
std::optional<Wall> Host::narrower_wall(lua_State* L, int idx) const {
    if (!lua_istable(L, idx)) {
        return std::nullopt;
    }
    std::vector<Rule> rules = config_.system_rules;
    bool asked = false;  // an empty list still asks: the command then reaches the system directories only
    for (const auto& [field, access] : {std::pair{"read", Access::Read}, std::pair{"write", Access::Write}}) {
        lua_getfield(L, idx, field);
        if (!lua_isnil(L, -1)) {
            if (!lua_istable(L, -1)) {
                luaL_error(L, "host.sh: %s is a list of paths", field);
            }
            asked = true;
            for (int i = 1; lua_rawgeti(L, -1, i) != LUA_TNIL; ++i) {
                std::size_t length = 0;
                const char* path = lua_type(L, -1) == LUA_TSTRING ? lua_tolstring(L, -1, &length) : nullptr;
                if (path == nullptr || std::strlen(path) != length) {
                    luaL_error(L, "host.sh: %s[%d] is not a path", field, i);
                }
                rules.push_back({path, access, true});
                lua_pop(L, 1);
            }
            lua_pop(L, 1);
        }
        lua_pop(L, 1);
    }
    if (!asked) {
        return std::nullopt;
    }
    try {
        return Wall::prepare(rules);
    } catch (const SandboxError& error) {
        luaL_error(L, "host.sh: %s", error.what());
    }
}

// host.judge(line?) -> {answers}, pending: sends line, if given, and waits for the answer the judge owes. One line
// at a time: a line is refused while an answer is owed, so each submission is the project as it was when sent.
// A judge can take longer than a time limit allows, so the wait ends kJudgeSlack seconds before the host.timed limit
// in force: the answers so far come back, and the rest come with a later call. A stop raises, and nothing is lost.
int Host::judge(lua_State* L) {
    constexpr double kJudgeSlack = 2.0;
    constexpr std::size_t kMaxLine = std::size_t{64} << 10;
    std::size_t length = 0;
    const char* line = luaL_optlstring(L, 1, nullptr, &length);
    Host& host = of(L);
    if (!host.config_.judge.valid()) {
        luaL_error(L, "host.judge: no judge in this session (start hx with --judge KIT)");
    }
    if (line != nullptr && (std::memchr(line, '\n', length) != nullptr || length > kMaxLine)) {
        luaL_error(L, "host.judge: send one line, without a newline, of at most 64 KiB");
    }
    raise_if_signaled(L);
    const int fd = host.config_.judge.get();
    if (line != nullptr && host.judge_owed_ > 0) {
        luaL_error(L, "host.judge: the judge is still judging the last submission; wait for its verdict first");
    }
    if (line != nullptr) {
        if (!write_all(fd, std::string(line, length) + "\n")) {
            luaL_error(L, "host.judge: the judge is gone");
        }
        ++host.judge_owed_;
    }
    const double until = host.deadline_ > 0 ? host.deadline_ - kJudgeSlack : 0;
    std::array<char, kBufferSize> buffer{};
    lua_newtable(L);
    int answers = 0;
    for (;;) {
        for (std::size_t end = host.judge_in_.find('\n'); end != std::string::npos; end = host.judge_in_.find('\n')) {
            lua_pushlstring(L, host.judge_in_.data(), end);
            lua_rawseti(L, -2, ++answers);
            host.judge_in_.erase(0, end + 1);
            --host.judge_owed_;
        }
        raise_if_signaled(L);
        if (host.judge_owed_ == 0 || (until > 0 && now() >= until)) {
            lua_pushinteger(L, host.judge_owed_);
            return 2;
        }
        pollfd waiter{fd, POLLIN, 0};
        wait_for({&waiter, 1}, until > 0 ? std::max(until - now(), 0.0) : -1.0);
        const ssize_t n = recv(fd, buffer.data(), buffer.size(), MSG_DONTWAIT);
        if (n == 0 || (n < 0 && errno != EAGAIN && errno != EINTR)) {
            luaL_error(L, "host.judge: the judge is gone");
        }
        if (n > 0) {
            host.judge_in_.append(buffer.data(), static_cast<std::size_t>(n));
        }
    }
}

double Host::within_limit(double seconds) const {
    seconds = duration(seconds);
    if (deadline_ <= 0) {
        return seconds;
    }
    return std::clamp(deadline_ - now(), 0.0, seconds);
}

int Host::sleep(lua_State* L) {
    raise_if_signaled(L);
    const double until = now() + of(L).within_limit(luaL_checknumber(L, 1));
    while (g_signal == 0) {
        const double left = until - now();
        if (left <= 0) {
            break;
        }
        wait_for({}, left);  // a signal ends it at once
    }
    raise_if_signaled(L);
    return 0;
}

// Calls the function at stack index fn with the arguments above it, under a time limit (0 for none), and returns
// true and its results or false and its error. This is where a stop ends: a call that returns while a stop is in
// progress returns false and the stop's error, whatever the function did, and the outermost such call also ends
// the stop.
int Host::protect(lua_State* L, int fn, double deadline) {
    luaL_checktype(L, fn, LUA_TFUNCTION);
    Host& host = of(L);
    int status = 0;
    {
        const DeadlineScope limit(host.deadline_, deadline);
        const TryScope scope(host.tries_);
        if (g_signal == 0) {  // a stop in progress does not start the function
            status = lua_pcall(L, lua_gettop(L) - fn, LUA_MULTRET, 0);
        }
    }
    if (const int sig = g_signal; sig != 0) {
        if (host.tries_ == 0) {
            g_signal = 0;
        }
        lua_settop(L, 0);
        lua_pushboolean(L, 0);
        lua_pushstring(L, stop_error(sig));
        return 2;
    }
    lua_pushboolean(L, status == 0 ? 1 : 0);
    lua_insert(L, fn);
    return lua_gettop(L) - fn + 1;
}

// host.try(fn, ...): a pcall that is also where a stop ends.
int Host::try_(lua_State* L) {
    return protect(L, 1, 0);
}

// host.timed(seconds, fn, ...): host.try whose callee is stopped with the error "timeout" once the limit passes.
// Limits nest: an inner call cannot outlast the outer one. llm, sh and sleep give up when the limit passes, and
// the error is raised as soon as Luau code runs again.
int Host::timed(lua_State* L) {
    const double seconds = duration(luaL_checknumber(L, 1));
    return protect(L, 2, now() + seconds);
}

void Host::open(lua_State* L) {
    lua_Callbacks* callbacks = lua_callbacks(L);
    callbacks->userdata = this;
    callbacks->interrupt = on_interrupt;

    static constexpr luaL_Reg kFunctions[] = {
        {"llm", llm},     {"tool", tool},     {"sh", sh},       {"try", try_},    {"timed", timed},
        {"read", l_read}, {"write", l_write}, {"list", l_list}, {"load", l_load}, {"input", l_input},
        {"log", l_log},   {"sleep", sleep},   {"env", l_env},   {"judge", judge}, {nullptr, nullptr},
    };
    luaL_register(L, "host", kFunctions);

    lua_createtable(L, static_cast<int>(config_.args.size()), 0);
    int index = 0;
    for (const std::string& arg : config_.args) {
        lua_pushlstring(L, arg.data(), arg.size());
        lua_rawseti(L, -2, ++index);
    }
    lua_setfield(L, -2, "args");

    std::error_code ec;
    const std::string cwd = std::filesystem::current_path(ec).string();
    lua_pushlstring(L, cwd.data(), cwd.size());
    lua_setfield(L, -2, "cwd");
    lua_pushboolean(L, config_.interactive ? 1 : 0);
    lua_setfield(L, -2, "interactive");
    lua_pushboolean(L, config_.quiet ? 1 : 0);
    lua_setfield(L, -2, "quiet");
    lua_pushboolean(L, sandboxed_ ? 1 : 0);
    lua_setfield(L, -2, "sandboxed");
    lua_pushlstring(L, config_.default_loop.data(), config_.default_loop.size());
    lua_setfield(L, -2, "default_loop");
    lua_pushboolean(L, isatty(STDOUT_FILENO) != 0 ? 1 : 0);
    lua_setfield(L, -2, "tty");
    lua_pushlstring(L, config_.model.data(), config_.model.size());
    lua_setfield(L, -2, "model");
    // host.tools speaks for DeepSeek's endpoints, which share one key, and host.exa for Exa's.
    const auto has_key = [this](std::string_view name) {
        const Endpoint* found = endpoint(name);
        return found != nullptr && !found->key.empty();
    };
    lua_pushboolean(L, has_key("search") ? 1 : 0);
    lua_setfield(L, -2, "tools");
    lua_pushboolean(L, has_key("exa") ? 1 : 0);
    lua_setfield(L, -2, "exa");
    lua_pushboolean(L, config_.judge.valid() ? 1 : 0);
    lua_setfield(L, -2, "judged");
    lua_createtable(L, 0, static_cast<int>(config_.extensions.size()));
    for (const auto& [name, source] : config_.extensions) {
        lua_pushlstring(L, source.data(), source.size());
        lua_setfield(L, -2, name.c_str());
    }
    lua_setfield(L, -2, "extensions");
    lua_pop(L, 1);
}

void install_signal_handlers() {
    struct sigaction action {};
    action.sa_handler = on_signal;
    sigemptyset(&action.sa_mask);
    action.sa_flags = 0;  // no SA_RESTART: a blocked read or wait has to return so the signal is noticed
    for (const int sig : {SIGINT, SIGTERM, SIGHUP}) {
        sigaction(sig, &action, nullptr);
    }
    // The time limit's timer. It only sets a flag for the VM, so system calls go on where they can.
    action.sa_handler = on_alarm;
    action.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &action, nullptr);
    static_cast<void>(std::signal(SIGPIPE, SIG_IGN));
}

}  // namespace hx
