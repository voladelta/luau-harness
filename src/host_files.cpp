#include <fcntl.h>
#include <poll.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
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

bool wait_ready(lua_State* L, int fd, short events, double until) {
    for (;;) {
        raise_if_signaled(L);
        const double left = until - now();
        if (left <= 0) {
            return false;
        }
        pollfd waiter{fd, events, 0};
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

}  // namespace

int Host::read_file(lua_State* L) {
    const char* path = check_cstring(L, 1);
    const double until = now() + of(L).within_limit(kLongest);
    const UniqueFd file(::open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC));
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

    struct stat st {};
    struct stat entry {};
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
    struct stat opened {};
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

}  // namespace hx
