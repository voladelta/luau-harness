#include "host.h"

#include <curl/curl.h>
#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/prctl.h>
#include <sys/random.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <sys/xattr.h>
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
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <fmt/format.h>

#include "lua.h"
#include "luacode.h"
#include "lualib.h"
#include "sys.h"

namespace hx {
namespace {

constexpr std::size_t kMaxFile = std::size_t{64} << 20;                // host.read refuses larger files
constexpr std::size_t kMaxResponse = std::size_t{256} << 20;           // host.llm and host.tool abort a larger reply
constexpr std::size_t kMaxUpload = std::size_t{100} << 20;             // the largest file DeepSeek's OCR takes
constexpr std::string_view kToolNames[] = {"search", "fetch", "ocr", "exa"};  // host.tool's endpoints
constexpr std::size_t kCaptureHalf = std::size_t{512} << 10;  // host.sh keeps this much of each end of the output
constexpr std::size_t kBufferSize = 65536;
constexpr int kPollMs = 50;
constexpr int kKillRounds = 100;          // at exit, so that a fork storm cannot hold hx up forever
constexpr std::size_t kMaxKilled = 4096;  // processes remembered at exit; past that, more rounds find the rest
constexpr double kReapSeconds = 1.0;      // how long hx waits for a process it killed to be gone
constexpr double kLongest = 1e9;          // seconds, about 30 years: the longest duration a call can ask for

// Signal handlers cannot be given context, so this state is global. It is the only mutable global state here.
// NOLINTBEGIN(cppcoreguidelines-avoid-non-const-global-variables)
volatile std::sig_atomic_t g_signal = 0;  // a stop in progress: the signal that asked for it
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
    TryScope(TryScope&&) = delete;
    TryScope& operator=(TryScope&&) = delete;
    ~TryScope() { --depth_; }

  private:
    int& depth_;
};

// Tightens a time limit for as long as the scope lives, then puts the outer limit back. A limit of 0 means none.
class DeadlineScope {
  public:
    DeadlineScope(double& slot, double limit) noexcept : slot_(slot), outer_(slot) {
        slot_ = limit <= 0 || (outer_ > 0 && outer_ < limit) ? outer_ : limit;
    }
    DeadlineScope(const DeadlineScope&) = delete;
    DeadlineScope& operator=(const DeadlineScope&) = delete;
    DeadlineScope(DeadlineScope&&) = delete;
    DeadlineScope& operator=(DeadlineScope&&) = delete;
    ~DeadlineScope() { slot_ = outer_; }

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

// Kills every descendant of hx, then waits a moment for them to be gone. A descendant is a child of hx or of a
// process killed here, so the whole tree is killed without waiting for any part of it to die: SIGKILL cannot end a
// process stuck in uninterruptible I/O (a dead NFS server) until the I/O returns, and hx does not wait on one
// longer than kReapSeconds. Nothing is reaped until every process is killed, so the zombies hold their PIDs and
// none can be reused by a process that is not a descendant.
void kill_descendants() noexcept {
    const pid_t self = getpid();
    std::array<pid_t, kMaxKilled> killed{};
    std::size_t count = 0;
    const auto was_killed = [&](pid_t pid) {
        return std::find(killed.begin(), killed.begin() + static_cast<std::ptrdiff_t>(count), pid) !=
               killed.begin() + static_cast<std::ptrdiff_t>(count);
    };
    for (int round = 0; round < kKillRounds; ++round) {
        const UniqueFd proc(open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC));
        if (!proc.valid()) {
            break;
        }
        bool found = false;
        alignas(dirent64) std::array<char, 4096> entries{};
        for (;;) {
            const ssize_t n = getdents64(proc.get(), entries.data(), entries.size());
            if (n <= 0) {
                break;
            }
            std::size_t at = 0;
            while (at < static_cast<std::size_t>(n)) {
                unsigned short size = 0;
                std::memcpy(&size, entries.data() + at + offsetof(dirent64, d_reclen), sizeof(size));
                const std::string_view name(entries.data() + at + offsetof(dirent64, d_name));
                at += size;
                pid_t pid = 0;
                const auto [stop, error] = std::from_chars(name.data(), name.data() + name.size(), pid);
                if (error != std::errc() || stop != name.data() + name.size() || was_killed(pid)) {
                    continue;
                }
                const pid_t parent = parent_of(proc.get(), name);
                if (parent != self && !was_killed(parent)) {
                    continue;
                }
                // Once kill returns, a fork in that process either has finished, so its child is listed by a
                // later scan, or fails.
                found = true;
                kill(pid, SIGKILL);
                if (count < killed.size()) {
                    killed.at(count) = pid;
                    ++count;
                }
            }
        }
        if (!found) {
            break;
        }
    }
    // A killed process whose children were killed too passes them to hx as it dies, so waiting for any child
    // reaps the whole tree.
    const double deadline = now() + kReapSeconds;
    for (;;) {
        const pid_t pid = waitpid(-1, nullptr, WNOHANG);
        if (pid > 0 || (pid < 0 && errno == EINTR)) {
            continue;
        }
        if (pid < 0 || now() >= deadline) {
            return;  // ECHILD: none left
        }
        const timespec nap{0, 10'000'000};
        nanosleep(&nap, nullptr);
    }
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

struct CurlEasyDeleter {
    void operator()(CURL* handle) const noexcept { curl_easy_cleanup(handle); }
};
using CurlEasy = std::unique_ptr<CURL, CurlEasyDeleter>;

// An owned list of request headers.
class Headers {
  public:
    Headers() = default;
    Headers(const Headers&) = delete;
    Headers& operator=(const Headers&) = delete;
    Headers(Headers&&) = delete;
    Headers& operator=(Headers&&) = delete;
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

// Sends a POST whose body is already set on curl, and returns status, text | nil, error. Redirects are not
// followed: a key goes to the URL it was meant for and nowhere else.
int post(lua_State* L, CURL* curl, const std::string& url, const Headers& headers, double timeout,
         const std::string& ca_bundle) {
    std::string response;
    std::array<char, CURL_ERROR_SIZE> error_buffer{};
    curl_easy_setopt(curl, CURLOPT_URL, url.c_str());
    curl_easy_setopt(curl, CURLOPT_PROTOCOLS_STR, "http,https");
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers.get());
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
    if (code != CURLE_OK) {
        raise_if_signaled(L);
        return fail(L, error_buffer.front() != '\0' ? error_buffer.data() : curl_easy_strerror(code));
    }
    long status = 0;
    curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &status);
    lua_pushnumber(L, static_cast<double>(status));
    lua_pushlstring(L, response.data(), response.size());
    return 2;
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

    [[nodiscard]] std::string text() const {
        if (dropped_ == 0) {
            return head_ + tail_;
        }
        return fmt::format("{}\n[... {} bytes of output omitted ...]\n{}", head_, dropped_, tail_);
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
[[noreturn]] void exec_bash(const char* command, int output) {
    // A session of its own: one process group to kill, and no terminal to ask a question on.
    setsid();
    const int null = open("/dev/null", O_RDONLY);
    if (null >= 0) {
        dup2(null, STDIN_FILENO);
    }
    dup2(output, STDOUT_FILENO);
    dup2(output, STDERR_FILENO);
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

// Reads a list or a value whose size is not known: asks for the size, then reads, and again if it grew meanwhile.
template <typename Get>
bool read_sized(const Get& get, std::string& out) {
    for (;;) {
        ssize_t n = get(nullptr, 0);
        if (n < 0) {
            return false;
        }
        out.resize(static_cast<std::size_t>(n));
        n = get(out.data(), out.size());
        if (n >= 0) {
            out.resize(static_cast<std::size_t>(n));
            return true;
        }
        if (errno != ERANGE) {
            return false;
        }
    }
}

// The extended attribute names in a list from listxattr, which ends each one with a NUL.
std::vector<std::string_view> attribute_names(std::string_view list) {
    std::vector<std::string_view> names;
    while (!list.empty()) {
        const std::size_t end = std::min(list.find('\0'), list.size());
        names.push_back(list.substr(0, end));
        list.remove_prefix(std::min(end + 1, list.size()));
    }
    return names;
}

// Gives the new file at `to` the extended attributes of the file at `from`, ACLs included, and none it has of its
// own. A file system without them has none to copy. False when one cannot be read or set (an SELinux label this
// user may not give, a trusted.* attribute): the new file could not stand in for the old one.
bool copy_attributes(const char* from, int to) {
    std::string list;
    std::string own;
    if (!read_sized([&](char* b, std::size_t s) { return listxattr(from, b, s); }, list)) {
        if (errno != ENOTSUP) {
            return false;
        }
        list.clear();
    }
    if (!read_sized([&](char* b, std::size_t s) { return flistxattr(to, b, s); }, own)) {
        if (errno != ENOTSUP) {
            return false;
        }
        own.clear();
    }
    const std::vector<std::string_view> names = attribute_names(list);
    std::string value;
    std::string current;
    for (const std::string_view view : names) {
        const std::string name(view);
        // Writing to a file drops its capabilities, so a new file does not take them over.
        if (name == "security.capability") {
            continue;
        }
        if (!read_sized([&](char* b, std::size_t s) { return getxattr(from, name.c_str(), b, s); }, value)) {
            return false;
        }
        if (read_sized([&](char* b, std::size_t s) { return fgetxattr(to, name.c_str(), b, s); }, current) &&
            current == value) {
            continue;  // the label or default ACL the new file was born with is already the same
        }
        if (fsetxattr(to, name.c_str(), value.data(), value.size(), 0) != 0) {
            return false;
        }
    }
    // Whatever the new file has that the old one did not, such as an ACL from its directory's default, goes.
    return std::ranges::all_of(attribute_names(own), [&](std::string_view view) {
        return std::ranges::find(names, view) != names.end() || fremovexattr(to, std::string(view).c_str()) == 0;
    });
}

// Gives an unnamed file (O_TMPFILE) the name `temp`, a free name of the form .NAME.hx-XXXXXX beside target, and
// stores that name in temp. False when the file cannot be linked: no /proc, or a sandbox that refuses it.
bool link_unnamed(int file, const std::filesystem::path& target, std::string& temp) {
    constexpr std::string_view kLetters = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    const std::string source = fmt::format("/proc/self/fd/{}", file);
    for (int attempt = 0; attempt < 100; ++attempt) {
        std::array<unsigned char, 6> random{};
        if (getrandom(random.data(), random.size(), 0) != static_cast<ssize_t>(random.size())) {
            return false;
        }
        std::string suffix;
        for (const unsigned char byte : random) {
            suffix.push_back(kLetters[byte % kLetters.size()]);
        }
        temp = (target.parent_path() / fmt::format(".{}.hx-{}", target.filename().string(), suffix)).string();
        if (linkat(AT_FDCWD, source.c_str(), AT_FDCWD, temp.c_str(), AT_SYMLINK_FOLLOW) == 0) {
            return true;
        }
        if (errno != EEXIST) {
            return false;
        }
    }
    return false;
}

enum class Replaced : std::uint8_t {
    Yes,
    Unable,  // this way cannot replace the file; nothing was changed
    Failed,  // errno says why
};

// Replaces target with a new file holding data, which gets old's owner, group, mode and extended attributes when
// old is given. An unnamed file (O_TMPFILE) has no name until it is complete, so a killed hx leaves nothing
// behind; a named one (mkostemp) serves where the file system or the sandbox refuses an unnamed one, and is left
// behind by a hx killed while writing it.
Replaced replace_file(const std::filesystem::path& target, const struct stat* old, std::string_view data,
                      bool unnamed) {
    const std::filesystem::path parent = target.parent_path().empty() ? "." : target.parent_path();
    std::string temp = (target.parent_path() / fmt::format(".{}.hx-XXXXXX", target.filename().string())).string();
    // A new file is created as open would create it, umask and default ACL alike.
    const UniqueFd file(unnamed ? open(parent.c_str(), O_TMPFILE | O_WRONLY | O_CLOEXEC, old != nullptr ? 0600 : 0666)
                                : mkostemp(temp.data(), O_CLOEXEC));
    if (!file.valid()) {
        return Replaced::Unable;
    }
    bool named = !unnamed;  // a name to remove on every way out but the rename
    const auto discard = [&](Replaced result) {
        const int error = errno;
        if (named) {
            unlink(temp.c_str());
        }
        errno = error;
        return result;
    };
    if (old != nullptr) {
        if (fchown(file.get(), old->st_uid, old->st_gid) != 0 || fchmod(file.get(), old->st_mode & 07777) != 0 ||
            !copy_attributes(target.c_str(), file.get())) {
            return discard(Replaced::Unable);
        }
    } else if (!unnamed && fchmod(file.get(), new_file_mode()) != 0) {
        return discard(Replaced::Unable);
    }
    if (!write_all(file.get(), data)) {
        return discard(Replaced::Failed);
    }
    if (unnamed) {
        if (!link_unnamed(file.get(), target, temp)) {
            return Replaced::Unable;
        }
        named = true;
    }
    if (rename(temp.c_str(), target.c_str()) != 0) {
        return discard(Replaced::Failed);
    }
    return Replaced::Yes;
}

// A file is written by writing a new file beside it and renaming that over it, so a full disk or a killed hx leaves
// the old content or the new, never a file cut short. Nothing is synced: a power loss can still lose both, and
// that is not what this guards against. Where a new file could not stand in for the old one, the file is written
// in place instead: something other than a regular file (/dev/null, a FIFO), a file with other names that must go
// on seeing it, a dangling link, a directory that refuses a new file (a lone --rw file), a file whose owner,
// group or extended attributes the new one cannot be given, or a file that may not be written (a rename would not
// ask it, so the open is left to refuse).
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
        // A link is followed, so the file it names is replaced and the link stays.
        const std::filesystem::path target = exists ? std::filesystem::canonical(path, ec) : path;
        for (const bool unnamed : {true, false}) {
            if (ec) {
                break;
            }
            const Replaced result = replace_file(target, exists ? &st : nullptr, data, unnamed);
            if (result == Replaced::Yes) {
                lua_pushboolean(L, 1);
                return 1;
            }
            if (result == Replaced::Failed) {
                return fail(L, fmt::format("{}: {}", path, error_text(errno)));
            }
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
// limit always reaches the code that is running.
void Host::on_interrupt(lua_State* L, int gc) {
    if (gc >= 0) {
        return;  // inside the collector: raising here is not allowed
    }
    raise_if_signaled(L);
    const Host& host = of(L);
    if (host.deadline_ > 0 && now() >= host.deadline_) {
        lua_pushstring(L, "timeout");
        lua_error(L);
    }
}

int Host::llm(lua_State* L) {
    std::size_t length = 0;
    const char* body = luaL_checklstring(L, 1, &length);
    const Host& host = of(L);
    const double timeout = host.within_limit(option(L, 2, "timeout", 600));
    raise_if_signaled(L);  // a stop in progress starts no request
    const HostConfig& config = host.config_;

    const CurlEasy curl(curl_easy_init());
    if (!curl) {
        return fail(L, "cannot start an HTTP request");
    }
    const std::string beta = beta_option(L, 2);
    Headers headers;
    headers.add("Content-Type: application/json");
    headers.add("anthropic-version: 2023-06-01");
    if (!beta.empty()) {
        headers.add(fmt::format("anthropic-beta: {}", beta));
    }
    if (!config.api_key.empty()) {
        headers.add(fmt::format("x-api-key: {}", config.api_key));
    }
    curl_easy_setopt(curl.get(), CURLOPT_POSTFIELDS, body);
    curl_easy_setopt(curl.get(), CURLOPT_POSTFIELDSIZE_LARGE, static_cast<curl_off_t>(length));
    return post(L, curl.get(), config.api_url, headers, timeout, config.ca_bundle);
}

// host.tool(name, request, {timeout=300}): a request to one of DeepSeek's tool endpoints, HX_TOOLS_URL/NAME, with
// DEEPSEEK_API_KEY as a bearer token, or, for "exa", to Exa's search endpoint, HX_EXA_URL, with EXA_API_KEY as
// x-api-key. Only the names in kToolNames are taken, so each key reaches its own endpoints and nothing else. A
// string request is sent as JSON; a table is sent as a multipart form, in which `file` names a file to upload and
// every other field is a text field. Exa takes JSON only.
int Host::tool(lua_State* L) {
    const std::string_view name = check_cstring(L, 1);
    if (std::ranges::find(kToolNames, name) == std::end(kToolNames)) {
        luaL_error(L, "host.tool: no tool %s; the tools are search, fetch, ocr and exa", std::string(name).c_str());
    }
    const bool exa = name == "exa";
    const bool form = !exa && lua_istable(L, 2);
    if (!form) {
        luaL_checktype(L, 2, LUA_TSTRING);
    }
    const Host& host = of(L);
    const double timeout = host.within_limit(option(L, 3, "timeout", 300));
    raise_if_signaled(L);
    const HostConfig& config = host.config_;
    if (exa ? config.exa_key.empty() : config.tools_key.empty()) {
        return fail(L, exa ? "no EXA_API_KEY: hx was started without one, so Exa cannot be used"
                           : "no DEEPSEEK_API_KEY: hx was started without one, so the DeepSeek tools cannot be used");
    }

    const CurlEasy curl(curl_easy_init());
    if (!curl) {
        return fail(L, "cannot start an HTTP request");
    }
    Headers headers;
    headers.add(exa ? fmt::format("x-api-key: {}", config.exa_key)
                    : fmt::format("Authorization: Bearer {}", config.tools_key));
    const CurlMime mime(form ? curl_mime_init(curl.get()) : nullptr);
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
        curl_easy_setopt(curl.get(), CURLOPT_MIMEPOST, mime.get());
    } else {
        std::size_t length = 0;
        const char* body = lua_tolstring(L, 2, &length);
        headers.add("Content-Type: application/json");
        curl_easy_setopt(curl.get(), CURLOPT_POSTFIELDS, body);
        curl_easy_setopt(curl.get(), CURLOPT_POSTFIELDSIZE_LARGE, static_cast<curl_off_t>(length));
    }
    const std::string url = exa ? config.exa_url : fmt::format("{}/{}", config.tools_url, name);
    return post(L, curl.get(), url, headers, timeout, config.ca_bundle);
}

int Host::sh(lua_State* L) {
    const char* command = check_cstring(L, 1);
    const Host& host = of(L);
    const double timeout = host.within_limit(option(L, 2, "timeout", 120));
    raise_if_signaled(L);  // a stop in progress starts no command
    // Adopted orphans that have exited are zombies of hx. No command of hx's own is running now, so this cannot
    // take the status of one.
    while (waitpid(-1, nullptr, WNOHANG) > 0) {
    }

    std::array<int, 2> pipe_fds{};
    if (pipe2(pipe_fds.data(), O_CLOEXEC) != 0) {
        luaL_error(L, "pipe: %s", error_text().c_str());
    }
    UniqueFd output(pipe_fds[0]);
    UniqueFd input(pipe_fds[1]);
    const pid_t pid = fork();
    if (pid < 0) {
        luaL_error(L, "fork: %s", error_text().c_str());
    }
    if (pid == 0) {
        exec_bash(command, input.get());
    }
    input.reset();
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
    enum class Ending : std::uint8_t { Exited, TimedOut, Signaled };
    const double deadline = now() + timeout;
    int status = 0;
    bool reaped = true;  // false: killed, but stuck in uninterruptible I/O and not yet dead
    Ending ending = Ending::Exited;
    for (;;) {
        drain();
        if (waitpid(pid, &status, WNOHANG) == pid) {
            break;
        }
        if (g_signal != 0 || now() >= deadline) {
            ending = g_signal != 0 ? Ending::Signaled : Ending::TimedOut;
            kill_group(pid);
            reaped = reap_within(pid, status, kReapSeconds);
            break;
        }
        // Wake on output, or after a short wait to notice the exit of a shell whose background job holds the pipe.
        pollfd waiter{output.get(), POLLIN, 0};
        poll(&waiter, at_end ? 0 : 1, kPollMs);
    }
    // bash is gone, or killed and left for a later reap. Whatever it left running keeps running, so do not wait
    // for the pipe to close.
    drain();
    if (ending == Ending::Signaled) {
        raise_if_signaled(L);
    }

    const std::string text = capture.text();
    lua_createtable(L, 0, 4);
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
    return 1;
}

double Host::within_limit(double seconds) const {
    seconds = duration(seconds);
    if (deadline_ <= 0) {
        return seconds;
    }
    return std::clamp(deadline_ - now(), 0.0, seconds);
}

int Host::sleep(lua_State* L) {
    constexpr double kStep = 0.1;  // short naps, so a signal ends the sleep promptly
    raise_if_signaled(L);
    const double until = now() + of(L).within_limit(luaL_checknumber(L, 1));
    while (g_signal == 0) {
        const double left = until - now();
        if (left <= 0) {
            break;
        }
        const timespec nap{0, static_cast<long>(std::min(left, kStep) * 1e9)};
        nanosleep(&nap, nullptr);
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
        {"llm", llm},     {"tool", tool},     {"sh", sh},       {"try", try_},      {"timed", timed},
        {"read", l_read}, {"write", l_write}, {"list", l_list}, {"load", l_load},   {"input", l_input},
        {"log", l_log},   {"sleep", sleep},   {"env", l_env},   {nullptr, nullptr},
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
    lua_pushboolean(L, config_.tools_key.empty() ? 0 : 1);
    lua_setfield(L, -2, "tools");
    lua_pushboolean(L, config_.exa_key.empty() ? 0 : 1);
    lua_setfield(L, -2, "exa");
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
    static_cast<void>(std::signal(SIGPIPE, SIG_IGN));
}

}  // namespace hx
