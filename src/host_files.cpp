// host.read, host.write and host.list: files, read whole and replaced whole.

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

constexpr std::size_t kMaxFile = std::size_t{64} << 20;  // host.read refuses larger files
constexpr double kNoReaderPoll = 0.05;                   // seconds between tries to open a FIFO that nobody reads yet

// Waits until fd is ready for events, a stop arrives (raised here) or the time `until` passes (false).
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

// What a FIFO or a device holds, read without blocking: a read that would block waits in wait_ready instead, where
// a stop and the time limit reach it. A FIFO opened before any writer came reads as ended at once, so it is not read
// until poll says it is readable, which it is once there is data or a writer has come and gone.
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

// Opens path to write in place, without blocking: a FIFO's open would otherwise wait for a reader where nothing can
// reach it. A FIFO that nobody reads refuses such an open (ENXIO), and nothing says when a reader comes, so it is
// tried again every kNoReaderPoll until `until`.
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

// Makes a regular file opened with O_NONBLOCK block again, so that it is read and written as it always was.
void block(int fd) {
    fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) & ~O_NONBLOCK);
}

// "nil, PATH: error", where a wait that reached the time limit says so instead of naming a network error.
int fail_wait(lua_State* L, std::string_view path) {
    return errno == ETIMEDOUT ? fail(L, fmt::format("{}: timed out", path)) : fail_errno(L, path);
}

// Writes all of data to a FIFO or a device opened without blocking, waiting for room until `until`.
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

}  // namespace

// A regular file is read as it is. What is not one (a FIFO, a device) can keep a read waiting for a writer, or for
// more, without end, so it is opened and read without blocking, and the waits end with the host.timed limit in
// force or a stop, as host.sh's and host.sleep's do.
int Host::read_file(lua_State* L) {
    const char* path = check_cstring(L, 1);
    const double until = now() + of(L).within_limit(kLongest);
    // O_NONBLOCK: a FIFO's open would otherwise wait for a writer where nothing can reach it.
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

// A regular file is written by writing a new file beside it and renaming that over it, so a full disk or a failed
// write leaves the old content or the new, never a file cut short; a hx killed mid-write can leave the new file
// behind. Nothing is synced: a power loss can still lose both, and that is not what this guards against. The new
// file gets the old one's mode, not its owner or extended attributes: git is the way back. A link is followed, so
// the file it names is replaced and the link stays. What a new file cannot stand in for is written in place:
// something other than a regular file (/dev/null, a FIFO), a file with other names that must go on seeing it, a
// dangling link, a file in a directory that refuses a new file (a lone --rw file), and a file that may not be
// written (a rename would not ask it, so the open is left to refuse). A FIFO or a device is opened and written
// without blocking, as host.read reads it: waiting for a reader, or for room, ends with the time limit or a stop.
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
