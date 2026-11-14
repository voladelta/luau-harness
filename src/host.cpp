#include "host.h"

#include <curl/curl.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
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

constexpr std::size_t kMaxFile = std::size_t{64} << 20;       // host.read refuses larger files
constexpr std::size_t kMaxResponse = std::size_t{256} << 20;  // host.llm aborts a larger reply
constexpr std::size_t kCaptureHalf = std::size_t{512} << 10;  // host.sh keeps this much of each end of the output
constexpr std::size_t kBufferSize = 65536;
constexpr int kPollMs = 50;
constexpr int kKillRounds = 100;  // at exit, so that a fork storm cannot hold hx up forever
constexpr double kLongest = 1e9;  // seconds, about 30 years: the longest duration a call can ask for

// Signal handlers cannot be given context, so this state is global. It is the only mutable global state here.
// NOLINTBEGIN(cppcoreguidelines-avoid-non-const-global-variables)
volatile std::sig_atomic_t g_signal = 0;      // a stop in progress: the signal that asked for it
volatile std::sig_atomic_t g_foreground = 0;  // process group of the command host.sh is waiting for
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

// Names the process group a second, impatient signal should kill, for as long as the scope lives.
class ForegroundScope {
  public:
    explicit ForegroundScope(pid_t group) noexcept { g_foreground = group; }
    ForegroundScope(const ForegroundScope&) = delete;
    ForegroundScope& operator=(const ForegroundScope&) = delete;
    ForegroundScope(ForegroundScope&&) = delete;
    ForegroundScope& operator=(ForegroundScope&&) = delete;
    ~ForegroundScope() { g_foreground = 0; }
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

void on_signal(int sig) {
    if (g_signal != 0) {
        // The first stop has not reached the end of its scope yet, so the loop is stuck: leave now.
        if (g_foreground > 0) {
            kill(-g_foreground, SIGKILL);
        }
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

// The parent of a process, read from /proc/PID/stat: "PID (COMM) STATE PPID ...". COMM can hold spaces and
// parentheses, so the fields are counted from the last ')'. Returns 0 when that cannot be read.
pid_t parent_of(pid_t pid) {
    const UniqueFd file(open(fmt::format("/proc/{}/stat", pid).c_str(), O_RDONLY | O_CLOEXEC));
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

// The processes whose parent is hx, exited ones that have not been reaped included.
std::vector<pid_t> children() {
    const pid_t self = getpid();
    std::vector<pid_t> found;
    std::error_code ec;
    for (std::filesystem::directory_iterator it("/proc", ec), end; !ec && it != end; it.increment(ec)) {
        const std::string name = it->path().filename().string();
        const char* last = name.data() + name.size();
        pid_t pid = 0;
        const auto [stop, error] = std::from_chars(name.data(), last, pid);
        if (error == std::errc() && stop == last && parent_of(pid) == self) {
            found.push_back(pid);
        }
    }
    return found;
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

int l_write(lua_State* L) {
    const char* path = check_cstring(L, 1);
    std::size_t length = 0;
    const char* raw = luaL_checklstring(L, 2, &length);
    std::string_view data(raw, length);

    std::error_code ec;
    const std::filesystem::path parent = std::filesystem::path(path).parent_path();
    if (!parent.empty()) {
        std::filesystem::create_directories(parent, ec);
        if (ec) {
            return fail(L, fmt::format("{}: {}", parent.string(), ec.message()));
        }
    }
    const UniqueFd file(open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0666));
    if (!file.valid()) {
        return fail_errno(L, path);
    }
    while (!data.empty()) {
        const ssize_t n = write(file.get(), data.data(), data.size());
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return fail_errno(L, path);
        }
        data.remove_prefix(static_cast<std::size_t>(n));
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
        raise_if_signaled(L);
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

// Kills every descendant. Killing a child orphans its children, which then become children of hx, so each round
// reaches one level further down the tree.
Host::~Host() {
    for (int round = 0; round < kKillRounds; ++round) {
        const std::vector<pid_t> doomed = children();
        if (doomed.empty()) {
            break;
        }
        for (const pid_t pid : doomed) {
            kill(pid, SIGKILL);
        }
        for (const pid_t pid : doomed) {
            while (waitpid(pid, nullptr, 0) < 0 && errno == EINTR) {
            }
        }
    }
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
    Host& host = of(L);
    const double timeout = host.within_limit(option(L, 2, "timeout", 600));
    raise_if_signaled(L);  // a stop in progress starts no request
    HostConfig& config = host.config_;

    const CurlEasy curl(curl_easy_init());
    if (!curl) {
        return fail(L, "cannot start an HTTP request");
    }
    Headers headers;
    headers.add("Content-Type: application/json");
    headers.add("anthropic-version: 2023-06-01");
    if (!config.api_key.empty()) {
        headers.add(fmt::format("x-api-key: {}", config.api_key));
    }

    std::string response;
    std::array<char, CURL_ERROR_SIZE> error_buffer{};
    curl_easy_setopt(curl.get(), CURLOPT_URL, config.api_url.c_str());
    curl_easy_setopt(curl.get(), CURLOPT_PROTOCOLS_STR, "http,https");
    curl_easy_setopt(curl.get(), CURLOPT_HTTPHEADER, headers.get());
    curl_easy_setopt(curl.get(), CURLOPT_POSTFIELDS, body);
    curl_easy_setopt(curl.get(), CURLOPT_POSTFIELDSIZE_LARGE, static_cast<curl_off_t>(length));
    curl_easy_setopt(curl.get(), CURLOPT_WRITEFUNCTION, on_body);
    curl_easy_setopt(curl.get(), CURLOPT_WRITEDATA, &response);
    curl_easy_setopt(curl.get(), CURLOPT_XFERINFOFUNCTION, on_progress);
    curl_easy_setopt(curl.get(), CURLOPT_NOPROGRESS, 0L);
    curl_easy_setopt(curl.get(), CURLOPT_NOSIGNAL, 1L);
    curl_easy_setopt(curl.get(), CURLOPT_CONNECTTIMEOUT, 30L);
    // Zero would mean "no limit" to libcurl, so an exhausted limit becomes the shortest one.
    curl_easy_setopt(curl.get(), CURLOPT_TIMEOUT_MS, std::max(1L, static_cast<long>(timeout * 1000)));
    curl_easy_setopt(curl.get(), CURLOPT_ERRORBUFFER, error_buffer.data());
    // Redirects are not followed: the key goes to the configured endpoint and nowhere else.
    if (!config.ca_bundle.empty()) {
        curl_blob blob{};
        blob.data = config.ca_bundle.data();
        blob.len = config.ca_bundle.size();
        blob.flags = CURL_BLOB_NOCOPY;  // the bundle lives as long as the Host
        curl_easy_setopt(curl.get(), CURLOPT_CAINFO_BLOB, &blob);
    }

    const CURLcode code = curl_easy_perform(curl.get());
    if (code != CURLE_OK) {
        raise_if_signaled(L);
        return fail(L, error_buffer.front() != '\0' ? error_buffer.data() : curl_easy_strerror(code));
    }
    long status = 0;
    curl_easy_getinfo(curl.get(), CURLINFO_RESPONSE_CODE, &status);
    lua_pushnumber(L, static_cast<double>(status));
    lua_pushlstring(L, response.data(), response.size());
    return 2;
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

    const ForegroundScope foreground(pid);

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
    Ending ending = Ending::Exited;
    for (;;) {
        drain();
        if (waitpid(pid, &status, WNOHANG) == pid) {
            break;
        }
        if (g_signal != 0 || now() >= deadline) {
            ending = g_signal != 0 ? Ending::Signaled : Ending::TimedOut;
            kill_group(pid);
            while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {
            }
            break;
        }
        // Wake on output, or after a short wait to notice the exit of a shell whose background job holds the pipe.
        pollfd waiter{output.get(), POLLIN, 0};
        poll(&waiter, at_end ? 0 : 1, kPollMs);
    }
    // bash is gone. Whatever it left running keeps running, so do not wait for the pipe to close.
    drain();
    if (ending == Ending::Signaled) {
        raise_if_signaled(L);
    }

    const std::string text = capture.text();
    lua_createtable(L, 0, 4);
    lua_pushlstring(L, text.data(), text.size());
    lua_setfield(L, -2, "out");
    if (WIFEXITED(status)) {
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
        {"llm", llm},       {"sh", sh},       {"try", try_},      {"timed", timed},   {"read", l_read},
        {"write", l_write}, {"list", l_list}, {"load", l_load},   {"input", l_input}, {"log", l_log},
        {"sleep", sleep},   {"env", l_env},   {nullptr, nullptr},
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
    lua_pushboolean(L, sandboxed_ ? 1 : 0);
    lua_setfield(L, -2, "sandboxed");
    lua_pushlstring(L, config_.default_loop.data(), config_.default_loop.size());
    lua_setfield(L, -2, "default_loop");
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
