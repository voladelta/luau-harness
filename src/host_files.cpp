#include <fcntl.h>
#include <fnmatch.h>
#include <poll.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <cmath>
#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <fmt/format.h>

#include "host.h"
#include "host_internal.h"
#include "lua.h"
#include "lualib.h"
#include "sys.h"

namespace hx {
namespace {

constexpr std::size_t kMaxFile = std::size_t{64} << 20;
constexpr double kNoReaderPoll = 0.05;
constexpr std::size_t kMaxWalk = 100000;

bool wait_ready(lua_State* L, int fd, short events, double until) {
    for (;;) {
        raise_if_signaled(L);
        const double left = until - now();
        if (left <= 0) {
            return false;
        }
        pollfd waiter{.fd = fd, .events = events, .revents = 0};
        wait_for({&waiter, 1}, left);
        if (waiter.revents != 0) {
            return true;
        }
    }
}

ReadAll read_waiting(lua_State* L, int fd, std::string& data, double until) {
    std::array<char, kBufferSize> buffer{};
    for (;;) {
        if (!wait_ready(L, fd, POLLIN, until)) {
            errno = ETIMEDOUT;
            return ReadAll::Failed;
        }
        const ssize_t n = ::read(fd, buffer.data(), buffer.size());
        if (n == 0) {
            return ReadAll::Done;
        }
        if (n < 0) {
            if (errno == EINTR || errno == EAGAIN) {
                continue;
            }
            return ReadAll::Failed;
        }
        if (data.size() + static_cast<std::size_t>(n) > kMaxFile) {
            return ReadAll::TooLarge;
        }
        data.append(buffer.data(), static_cast<std::size_t>(n));
    }
}

UniqueFd open_in_place(lua_State* L, const char* path, bool fifo, double until) {
    for (;;) {
        UniqueFd file(open(path, O_WRONLY | O_CREAT | O_TRUNC | O_NONBLOCK | O_CLOEXEC, 0666));
        if (file.valid() || errno != ENXIO || !fifo) {
            return file;
        }
        raise_if_signaled(L);
        const double left = until - now();
        if (left <= 0) {
            errno = ETIMEDOUT;
            return file;
        }
        wait_for({}, std::min(left, kNoReaderPoll));
    }
}

void block(int fd) {
    fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) & ~O_NONBLOCK);
}

int fail_wait(lua_State* L, std::string_view path) {
    return errno == ETIMEDOUT ? fail(L, fmt::format("{}: timed out", path)) : fail_errno(L, path);
}

bool write_waiting(lua_State* L, int fd, std::string_view data, double until) {
    while (!data.empty()) {
        const ssize_t n = ::write(fd, data.data(), data.size());
        if (n >= 0) {
            data.remove_prefix(static_cast<std::size_t>(n));
            continue;
        }
        if (errno != EINTR && errno != EAGAIN) {
            return false;
        }
        if (!wait_ready(L, fd, POLLOUT, until)) {
            errno = ETIMEDOUT;
            return false;
        }
    }
    return true;
}

mode_t new_file_mode() {
    const mode_t mask = umask(0);
    umask(mask);
    return 0666 & ~mask;
}

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

std::vector<std::string_view> segments(std::string_view path) {
    std::vector<std::string_view> parts;
    while (!path.empty()) {
        const std::size_t slash = path.find('/');
        const std::string_view part = path.substr(0, slash);
        if (!part.empty()) {
            parts.push_back(part);
        }
        path = slash == std::string_view::npos ? std::string_view{} : path.substr(slash + 1);
    }
    return parts;
}

bool matches(std::span<const std::string_view> glob, std::span<const std::string_view> path) {
    if (glob.empty()) {
        return path.empty();
    }
    if (glob.front() == "**") {
        if (glob.size() == 1) {
            return !path.empty();
        }
        for (std::size_t skip = 0; skip <= path.size(); ++skip) {
            if (matches(glob.subspan(1), path.subspan(skip))) {
                return true;
            }
        }
        return false;
    }
    if (path.empty()) {
        return false;
    }
    const std::string segment(glob.front());
    const std::string name(path.front());
    return fnmatch(segment.c_str(), name.c_str(), 0) == 0 && matches(glob.subspan(1), path.subspan(1));
}

const char* kind_of(mode_t mode) {
    if (S_ISREG(mode)) {
        return "file";
    }
    if (S_ISDIR(mode)) {
        return "dir";
    }
    if (S_ISLNK(mode)) {
        return "link";
    }
    if (S_ISFIFO(mode)) {
        return "fifo";
    }
    if (S_ISSOCK(mode)) {
        return "socket";
    }
    return "device";
}

}  // namespace

int Host::read_file(lua_State* L) {
    const char* path = check_cstring(L, 1);
    const double until = now() + of(L).within_limit(kLongest);
    const UniqueFd file(::open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC));
    if (!file.valid()) {
        return fail_errno(L, path);
    }
    struct stat st{};
    if (fstat(file.get(), &st) != 0) {
        return fail_errno(L, path);
    }
    if (S_ISDIR(st.st_mode)) {
        return fail(L, fmt::format("{}: is a directory", path));
    }
    std::string data;
    ReadAll read = ReadAll::Done;
    if (S_ISREG(st.st_mode)) {
        block(file.get());
        read = read_all(file.get(), data, kMaxFile);
    } else {
        read = read_waiting(L, file.get(), data, until);
    }
    switch (read) {
        case ReadAll::Done:
            break;
        case ReadAll::Failed:
            return fail_wait(L, path);
        case ReadAll::TooLarge:
            return fail(L, fmt::format("{}: larger than 64 MiB", path));
    }
    lua_pushlstring(L, data.data(), data.size());
    return 1;
}

int Host::write_file(lua_State* L) {
    const char* path = check_cstring(L, 1);
    const double until = now() + of(L).within_limit(kLongest);
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

    struct stat st{};
    struct stat entry{};
    const bool exists = stat(path, &st) == 0;
    const bool missing = !exists && errno == ENOENT && lstat(path, &entry) != 0;
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

    const UniqueFd file = open_in_place(L, path, exists && S_ISFIFO(st.st_mode), until);
    struct stat opened{};
    if (!file.valid() || fstat(file.get(), &opened) != 0) {
        return fail_wait(L, path);
    }
    bool written = false;
    if (S_ISREG(opened.st_mode)) {
        block(file.get());
        written = write_all(file.get(), data);
    } else {
        written = write_waiting(L, file.get(), data, until);
    }
    if (!written) {
        return fail_wait(L, path);
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
    push_strings(L, names);
    return 1;
}

int l_stat(lua_State* L) {
    const char* path = check_cstring(L, 1);
    struct stat st{};
    if (lstat(path, &st) != 0) {
        return fail_errno(L, path);
    }
    lua_createtable(L, 0, 5);
    lua_pushstring(L, kind_of(st.st_mode));
    lua_setfield(L, -2, "type");
    lua_pushnumber(L, static_cast<double>(st.st_size));
    lua_setfield(L, -2, "size");
    lua_pushnumber(L, static_cast<double>(st.st_mtim.tv_sec) + (static_cast<double>(st.st_mtim.tv_nsec) * 1e-9));
    lua_setfield(L, -2, "mtime");
    const std::string mode = fmt::format("{:o}", st.st_mode & 07777);
    lua_pushlstring(L, mode.data(), mode.size());
    lua_setfield(L, -2, "mode");
    if (S_ISLNK(st.st_mode)) {
        std::error_code ec;
        const std::string target = std::filesystem::read_symlink(path, ec).string();
        if (!ec) {
            lua_pushlstring(L, target.data(), target.size());
            lua_setfield(L, -2, "target");
        }
    }
    return 1;
}

int l_walk(lua_State* L) {
    const char* root = check_cstring(L, 1);
    const double depth = option(L, 2, "depth", kLongest);
    if (depth < 1 || std::floor(depth) != depth) {
        luaL_error(L, "host.walk: depth is a whole number, 1 or more");
    }
    std::string glob;
    if (lua_istable(L, 2)) {
        lua_getfield(L, 2, "glob");
        if (!lua_isnil(L, -1)) {
            glob = luaL_checkstring(L, -1);
        }
        lua_pop(L, 1);
    }
    const bool by_name = glob.find('/') == std::string::npos;
    const std::vector<std::string_view> pattern = segments(glob);

    std::error_code ec;
    std::vector<std::string> paths;
    const std::filesystem::path base(root);
    using Options = std::filesystem::directory_options;
    std::filesystem::recursive_directory_iterator it(base, Options::skip_permission_denied, ec);
    for (const std::filesystem::recursive_directory_iterator end; !ec && it != end; it.increment(ec)) {
        raise_if_signaled(L);
        std::error_code type_ec;
        const bool dir = it->is_directory(type_ec) && !it->is_symlink(type_ec);
        const std::string name = it->path().filename().string();
        if (dir && name == ".git") {
            it.disable_recursion_pending();
            continue;
        }
        if (dir && static_cast<double>(it.depth() + 1) >= depth) {
            it.disable_recursion_pending();
        }
        std::string relative = it->path().lexically_relative(base).generic_string();
        if (!glob.empty()) {
            const std::vector<std::string_view> parts = segments(relative);
            const bool hit = by_name ? fnmatch(glob.c_str(), name.c_str(), 0) == 0 : matches(pattern, parts);
            if (!hit) {
                continue;
            }
        }
        if (paths.size() == kMaxWalk) {
            return fail(L, fmt::format("{}: more than {} entries; narrow it with depth or glob", root, kMaxWalk));
        }
        if (dir) {
            relative.push_back('/');
        }
        paths.push_back(std::move(relative));
    }
    if (ec) {
        return fail(L, fmt::format("{}: {}", root, ec.message()));
    }
    std::sort(paths.begin(), paths.end());
    push_strings(L, paths);
    return 1;
}

}  // namespace hx
