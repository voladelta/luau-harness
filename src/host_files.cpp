// host.read, host.write and host.list: files, read whole and replaced whole.

#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
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

#include "host_internal.h"
#include "lua.h"
#include "lualib.h"
#include "sys.h"

namespace hx {
namespace {

constexpr std::size_t kMaxFile = std::size_t{64} << 20;  // host.read refuses larger files

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
    switch (read_all(file.get(), data, kMaxFile)) {
        case ReadAll::Done:
            break;
        case ReadAll::Failed:
            return fail_errno(L, path);
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
    push_strings(L, names);
    return 1;
}

}  // namespace hx
