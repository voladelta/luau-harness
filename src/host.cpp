#include "host.h"

#include <algorithm>
#include <cerrno>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <initializer_list>
#include <iostream>
#include <memory>
#include <system_error>

#include <curl/curl.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include "lua.h"
#include "luacode.h"
#include "lualib.h"

namespace hx {
namespace {

constexpr size_t kMaxFile = 64u << 20;      // host.read refuses larger files
constexpr size_t kMaxResponse = 256u << 20; // host.llm aborts a larger reply
constexpr size_t kCaptureHalf = 512u << 10; // host.sh keeps this much of the start and of the end of the output

const HostConfig* g_config = nullptr;
volatile std::sig_atomic_t g_signal = 0;     // a signal waiting to become a Luau error
volatile std::sig_atomic_t g_foreground = 0; // process group of the command host.sh is waiting for
double g_deadline = 0;                       // the innermost host.timed limit on the monotonic clock; 0 for none
std::vector<pid_t> g_groups;                 // process groups host.sh started

double now() {
    timespec ts{};
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return static_cast<double>(ts.tv_sec) + static_cast<double>(ts.tv_nsec) * 1e-9;
}

// The conventional "nil, message" return for a failure the caller is expected to handle.
int fail(lua_State* L, const std::string& message) {
    lua_pushnil(L);
    lua_pushlstring(L, message.data(), message.size());
    return 2;
}

int fail_errno(lua_State* L, const char* path) { return fail(L, std::string(path) + ": " + std::strerror(errno)); }

struct Fd {
    int fd;
    ~Fd() {
        if (fd >= 0) close(fd);
    }
};

// Reads an optional numeric field of the options table at idx.
double option(lua_State* L, int idx, const char* name, double fallback) {
    if (lua_isnoneornil(L, idx)) return fallback;
    luaL_checktype(L, idx, LUA_TTABLE);
    lua_getfield(L, idx, name);
    const double value = lua_isnil(L, -1) ? fallback : luaL_checknumber(L, -1);
    lua_pop(L, 1);
    return value;
}

void on_signal(int sig) {
    if (g_signal != 0) {
        // The first signal has not been turned into an error yet, so the loop is stuck: leave now.
        if (g_foreground > 0) kill(-g_foreground, SIGKILL);
        _exit(128 + sig);
    }
    g_signal = sig;
}

// Runs at VM safepoints (loop back edges, calls). Luau code cannot block these, so a signal or an expired
// time limit always reaches the code that is running.
void on_interrupt(lua_State* L, int gc) {
    if (gc >= 0) return; // inside the collector: raising here is not allowed
    if (g_signal != 0) {
        const int sig = g_signal;
        g_signal = 0;
        lua_pushstring(L, sig == SIGINT ? "interrupted" : "terminated");
        lua_error(L);
    }
    if (g_deadline > 0 && now() >= g_deadline) {
        lua_pushstring(L, "timeout");
        lua_error(L);
    }
}

// --- host.llm ---------------------------------------------------------------------------------------------

size_t on_body(char* data, size_t size, size_t count, void* user) noexcept {
    auto* body = static_cast<std::string*>(user);
    const size_t bytes = size * count;
    if (body->size() + bytes > kMaxResponse) return 0; // a short count aborts the transfer
    try {
        body->append(data, bytes);
    } catch (...) {
        return 0;
    }
    return bytes;
}

int on_progress(void*, curl_off_t, curl_off_t, curl_off_t, curl_off_t) noexcept { return g_signal != 0; }

struct CurlEasyDeleter {
    void operator()(CURL* handle) const { curl_easy_cleanup(handle); }
};
struct CurlListDeleter {
    void operator()(curl_slist* list) const { curl_slist_free_all(list); }
};

int l_llm(lua_State* L) {
    size_t length = 0;
    const char* body = luaL_checklstring(L, 1, &length);
    const double timeout = option(L, 2, "timeout", 600);
    const HostConfig& config = *g_config;

    const std::unique_ptr<CURL, CurlEasyDeleter> curl(curl_easy_init());
    if (!curl) return fail(L, "cannot start an HTTP request");

    curl_slist* raw_headers = curl_slist_append(nullptr, "Content-Type: application/json");
    raw_headers = curl_slist_append(raw_headers, "anthropic-version: 2023-06-01");
    if (!config.api_key.empty()) raw_headers = curl_slist_append(raw_headers, ("x-api-key: " + config.api_key).c_str());
    const std::unique_ptr<curl_slist, CurlListDeleter> headers(raw_headers);

    std::string response;
    char error_text[CURL_ERROR_SIZE] = "";
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
    curl_easy_setopt(curl.get(), CURLOPT_TIMEOUT_MS, static_cast<long>(timeout * 1000));
    curl_easy_setopt(curl.get(), CURLOPT_ERRORBUFFER, error_text);
    // Redirects are not followed: the key goes to the configured endpoint and nowhere else.
    if (!config.ca_bundle.empty()) {
        curl_blob blob{};
        blob.data = const_cast<char*>(config.ca_bundle.data());
        blob.len = config.ca_bundle.size();
        blob.flags = CURL_BLOB_COPY;
        curl_easy_setopt(curl.get(), CURLOPT_CAINFO_BLOB, &blob);
    }

    const CURLcode code = curl_easy_perform(curl.get());
    if (code != CURLE_OK) {
        if (g_signal != 0) return fail(L, "interrupted");
        return fail(L, error_text[0] != '\0' ? error_text : curl_easy_strerror(code));
    }
    long status = 0;
    curl_easy_getinfo(curl.get(), CURLINFO_RESPONSE_CODE, &status);
    lua_pushnumber(L, static_cast<double>(status));
    lua_pushlstring(L, response.data(), response.size());
    return 2;
}

// --- host.sh ----------------------------------------------------------------------------------------------

// Keeps the first and the last part of a stream and counts what fell out of the middle: the start of a command's
// output says what ran, the end says how it failed.
class Capture {
  public:
    void add(const char* data, size_t size) {
        if (head_.size() < kCaptureHalf) {
            const size_t take = std::min(size, kCaptureHalf - head_.size());
            head_.append(data, take);
            data += take;
            size -= take;
        }
        if (size == 0) return;
        tail_.append(data, size);
        if (tail_.size() > 2 * kCaptureHalf) {
            const size_t cut = tail_.size() - kCaptureHalf;
            dropped_ += cut;
            tail_.erase(0, cut);
        }
    }

    std::string text() const {
        if (dropped_ == 0) return head_ + tail_;
        return head_ + "\n[... " + std::to_string(dropped_) + " bytes of output omitted ...]\n" + tail_;
    }

  private:
    std::string head_;
    std::string tail_;
    size_t dropped_ = 0;
};

void kill_group(pid_t group) {
    // Until the child has called setsid() its group does not exist yet; then the child itself is the target.
    if (kill(-group, SIGKILL) != 0) kill(group, SIGKILL);
}

int l_sh(lua_State* L) {
    const char* command = luaL_checkstring(L, 1);
    const double timeout = option(L, 2, "timeout", 120);

    int pipe_fds[2];
    if (pipe2(pipe_fds, O_CLOEXEC) != 0) luaL_error(L, "pipe: %s", std::strerror(errno));
    const pid_t pid = fork();
    if (pid < 0) {
        const int saved = errno;
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        luaL_error(L, "fork: %s", std::strerror(saved));
    }
    if (pid == 0) {
        // A session of its own: one process group to kill, and no terminal to ask a question on.
        setsid();
        const int null = open("/dev/null", O_RDONLY);
        if (null >= 0) dup2(null, STDIN_FILENO);
        dup2(pipe_fds[1], STDOUT_FILENO);
        dup2(pipe_fds[1], STDERR_FILENO);
        for (const int sig : {SIGINT, SIGTERM, SIGHUP, SIGPIPE}) signal(sig, SIG_DFL);
        execlp("bash", "bash", "-c", command, static_cast<char*>(nullptr));
        dprintf(STDERR_FILENO, "hx: cannot run bash: %s\n", std::strerror(errno));
        _exit(127);
    }
    close(pipe_fds[1]);
    const Fd output{pipe_fds[0]};
    fcntl(output.fd, F_SETFL, O_NONBLOCK);

    std::erase_if(g_groups, [](pid_t group) { return kill(-group, 0) != 0 && errno == ESRCH; });
    g_groups.push_back(pid);
    g_foreground = pid;

    Capture capture;
    char buffer[65536];
    bool at_end = false; // every writer has closed the pipe
    const auto drain = [&] {
        while (!at_end) {
            const ssize_t n = read(output.fd, buffer, sizeof buffer);
            if (n > 0) capture.add(buffer, static_cast<size_t>(n));
            else if (n == 0) at_end = true;
            else if (errno != EINTR) break; // EAGAIN: nothing more for now
        }
    };

    const double deadline = now() + timeout;
    int status = 0;
    bool timed_out = false;
    for (;;) {
        drain();
        if (waitpid(pid, &status, WNOHANG) == pid) break;
        if (g_signal != 0 || now() >= deadline) {
            timed_out = g_signal == 0;
            kill_group(pid);
            while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {}
            break;
        }
        // Wake on output, or every 50 ms to notice the exit of a shell whose background job holds the pipe.
        pollfd waiter{output.fd, POLLIN, 0};
        poll(&waiter, at_end ? 0 : 1, 50);
    }
    // bash is gone. Whatever it left running keeps running, so do not wait for the pipe to close.
    drain();
    g_foreground = 0;

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
    lua_pushboolean(L, timed_out);
    lua_setfield(L, -2, "timed_out");
    return 1;
}

// --- files ------------------------------------------------------------------------------------------------

int l_read(lua_State* L) {
    const char* path = luaL_checkstring(L, 1);
    const Fd file{open(path, O_RDONLY | O_CLOEXEC)};
    if (file.fd < 0) return fail_errno(L, path);
    struct stat st {};
    if (fstat(file.fd, &st) != 0) return fail_errno(L, path);
    if (S_ISDIR(st.st_mode)) return fail(L, std::string(path) + ": is a directory");

    std::string data;
    char buffer[65536];
    for (;;) {
        const ssize_t n = read(file.fd, buffer, sizeof buffer);
        if (n == 0) break;
        if (n < 0) {
            if (errno == EINTR) continue;
            return fail_errno(L, path);
        }
        if (data.size() + static_cast<size_t>(n) > kMaxFile) return fail(L, std::string(path) + ": larger than 64 MiB");
        data.append(buffer, static_cast<size_t>(n));
    }
    lua_pushlstring(L, data.data(), data.size());
    return 1;
}

int l_write(lua_State* L) {
    const char* path = luaL_checkstring(L, 1);
    size_t length = 0;
    const char* data = luaL_checklstring(L, 2, &length);

    std::error_code ec;
    const std::filesystem::path parent = std::filesystem::path(path).parent_path();
    if (!parent.empty()) {
        std::filesystem::create_directories(parent, ec);
        if (ec) return fail(L, parent.string() + ": " + ec.message());
    }
    const Fd file{open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0666)};
    if (file.fd < 0) return fail_errno(L, path);
    for (size_t done = 0; done < length;) {
        const ssize_t n = write(file.fd, data + done, length - done);
        if (n < 0) {
            if (errno == EINTR) continue;
            return fail_errno(L, path);
        }
        done += static_cast<size_t>(n);
    }
    lua_pushboolean(L, 1);
    return 1;
}

int l_list(lua_State* L) {
    const char* path = luaL_checkstring(L, 1);
    std::error_code ec;
    std::vector<std::string> names;
    for (std::filesystem::directory_iterator it(path, ec), end; !ec && it != end; it.increment(ec)) {
        std::string name = it->path().filename().string();
        std::error_code type_ec;
        if (it->is_directory(type_ec)) name.push_back('/');
        names.push_back(std::move(name));
    }
    if (ec) return fail(L, std::string(path) + ": " + ec.message());
    std::sort(names.begin(), names.end());
    lua_createtable(L, static_cast<int>(names.size()), 0);
    for (size_t i = 0; i < names.size(); ++i) {
        lua_pushlstring(L, names[i].data(), names[i].size());
        lua_rawseti(L, -2, static_cast<int>(i + 1));
    }
    return 1;
}

// --- the VM itself ----------------------------------------------------------------------------------------

struct FreeDeleter {
    void operator()(char* p) const { std::free(p); }
};

int l_load(lua_State* L) {
    size_t length = 0;
    const char* source = luaL_checklstring(L, 1, &length);
    const char* name = luaL_optstring(L, 2, "=chunk");
    const bool has_env = !lua_isnoneornil(L, 3);
    if (has_env) luaL_checktype(L, 3, LUA_TTABLE);

    size_t bytecode_size = 0;
    const std::unique_ptr<char, FreeDeleter> bytecode(luau_compile(source, length, nullptr, &bytecode_size));
    if (!bytecode) luaL_error(L, "out of memory compiling %s", name);
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

// host.timed(seconds, fn, ...): a pcall whose callee is stopped with the error "timeout" once the limit passes.
// Limits nest: an inner call cannot outlast the outer one. Time spent blocked in a host function counts, but
// the error is only raised once Luau code runs again.
int l_timed(lua_State* L) {
    const double seconds = luaL_checknumber(L, 1);
    luaL_checktype(L, 2, LUA_TFUNCTION);
    const double outer = g_deadline;
    const double mine = now() + seconds;
    g_deadline = outer > 0 && outer < mine ? outer : mine;
    const int status = lua_pcall(L, lua_gettop(L) - 2, LUA_MULTRET, 0);
    g_deadline = outer;
    lua_pushboolean(L, status == 0);
    lua_replace(L, 1);
    return lua_gettop(L);
}

// --- small things -----------------------------------------------------------------------------------------

int l_input(lua_State* L) {
    const char* prompt = luaL_optstring(L, 1, "");
    std::fputs(prompt, stderr);
    std::fflush(stderr);
    std::string line;
    if (!std::getline(std::cin, line)) return 0;
    lua_pushlstring(L, line.data(), line.size());
    return 1;
}

int l_log(lua_State* L) {
    size_t length = 0;
    const char* text = luaL_checklstring(L, 1, &length);
    std::fwrite(text, 1, length, stderr);
    std::fputc('\n', stderr);
    return 0;
}

int l_sleep(lua_State* L) {
    const double until = now() + luaL_checknumber(L, 1);
    for (double left = until - now(); left > 0 && g_signal == 0; left = until - now()) {
        const double step = std::min(left, 0.1);
        const timespec ts{0, static_cast<long>(step * 1e9)};
        nanosleep(&ts, nullptr);
    }
    return 0;
}

int l_env(lua_State* L) {
    const char* value = std::getenv(luaL_checkstring(L, 1));
    if (value == nullptr) return 0;
    lua_pushstring(L, value);
    return 1;
}

} // namespace

void open_host(lua_State* L, const HostConfig& config) {
    g_config = &config;
    lua_callbacks(L)->interrupt = on_interrupt;

    static const luaL_Reg functions[] = {
        {"llm", l_llm},     {"sh", l_sh},       {"read", l_read},   {"write", l_write},
        {"list", l_list},   {"load", l_load},   {"timed", l_timed}, {"input", l_input},
        {"log", l_log},     {"sleep", l_sleep}, {"env", l_env},     {nullptr, nullptr},
    };
    luaL_register(L, "host", functions);

    lua_createtable(L, static_cast<int>(config.args.size()), 0);
    for (size_t i = 0; i < config.args.size(); ++i) {
        lua_pushlstring(L, config.args[i].data(), config.args[i].size());
        lua_rawseti(L, -2, static_cast<int>(i + 1));
    }
    lua_setfield(L, -2, "args");

    std::error_code ec;
    const std::string cwd = std::filesystem::current_path(ec).string();
    lua_pushlstring(L, cwd.data(), cwd.size());
    lua_setfield(L, -2, "cwd");
    lua_pushboolean(L, config.interactive);
    lua_setfield(L, -2, "interactive");
    lua_pushlstring(L, config.default_loop.data(), config.default_loop.size());
    lua_setfield(L, -2, "default_loop");
    lua_pop(L, 1);
}

void install_signal_handlers() {
    struct sigaction action {};
    action.sa_handler = on_signal;
    sigemptyset(&action.sa_mask);
    action.sa_flags = 0; // no SA_RESTART: a blocked read or wait has to return so the signal is noticed
    for (const int sig : {SIGINT, SIGTERM, SIGHUP}) sigaction(sig, &action, nullptr);
    signal(SIGPIPE, SIG_IGN);
}

void kill_children() {
    for (const pid_t group : g_groups) kill(-group, SIGKILL);
    g_groups.clear();
}

} // namespace hx
