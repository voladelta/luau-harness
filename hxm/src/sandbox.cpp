#include "sandbox.h"

#include <cerrno>
#include <cstdint>
#include <cstring>

#include <fcntl.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

namespace hxm {
namespace {

// The Landlock kernel interface (linux/landlock.h), restated here so the build does not depend on how recent
// the installed kernel headers are. These values are a stable kernel ABI.
constexpr uint64_t kExecute = 1ULL << 0;
constexpr uint64_t kWriteFile = 1ULL << 1;
constexpr uint64_t kReadFile = 1ULL << 2;
constexpr uint64_t kReadDir = 1ULL << 3;
constexpr uint64_t kRemoveDir = 1ULL << 4;
constexpr uint64_t kRemoveFile = 1ULL << 5;
constexpr uint64_t kMakeChar = 1ULL << 6;
constexpr uint64_t kMakeDir = 1ULL << 7;
constexpr uint64_t kMakeReg = 1ULL << 8;
constexpr uint64_t kMakeSock = 1ULL << 9;
constexpr uint64_t kMakeFifo = 1ULL << 10;
constexpr uint64_t kMakeBlock = 1ULL << 11;
constexpr uint64_t kMakeSym = 1ULL << 12;
constexpr uint64_t kRefer = 1ULL << 13;    // ABI 2
constexpr uint64_t kTruncate = 1ULL << 14; // ABI 3
constexpr uint64_t kIoctlDev = 1ULL << 15; // ABI 5

constexpr uint64_t kScopeAbstractUnixSocket = 1ULL << 0; // ABI 6
constexpr uint64_t kScopeSignal = 1ULL << 1;             // ABI 6

constexpr uint32_t kCreateRulesetVersion = 1U << 0;
constexpr int kRulePathBeneath = 1;

#ifndef SYS_landlock_create_ruleset
#define SYS_landlock_create_ruleset 444
#define SYS_landlock_add_rule 445
#define SYS_landlock_restrict_self 446
#endif

struct RulesetAttr {
    uint64_t handled_access_fs;
    uint64_t handled_access_net; // ABI 4; left at zero: the network stays open
    uint64_t scoped;             // ABI 6
};

struct [[gnu::packed]] PathBeneathAttr {
    uint64_t allowed_access;
    int32_t parent_fd;
};

constexpr uint64_t kRead = kExecute | kReadFile | kReadDir;
constexpr uint64_t kWrite = kRead | kWriteFile | kRemoveDir | kRemoveFile | kMakeDir | kMakeReg | kMakeSock |
                            kMakeFifo | kMakeSym | kRefer | kTruncate;
constexpr uint64_t kDevice = kReadFile | kReadDir | kWriteFile | kIoctlDev;
// The rights that mean something on a file; the kernel rejects a rule that puts any other on one.
constexpr uint64_t kFileRights = kExecute | kWriteFile | kReadFile | kTruncate | kIoctlDev;

// Every right this ABI knows. All of them are handled, so anything not granted by a rule is denied; device and
// block nodes can never be created because no rule grants kMakeChar or kMakeBlock.
uint64_t handled_rights(int abi) {
    uint64_t rights = kRead | kWriteFile | kRemoveDir | kRemoveFile | kMakeChar | kMakeDir | kMakeReg | kMakeSock |
                      kMakeFifo | kMakeBlock | kMakeSym;
    if (abi >= 2) rights |= kRefer;
    if (abi >= 3) rights |= kTruncate;
    if (abi >= 5) rights |= kIoctlDev;
    return rights;
}

uint64_t rights_of(Access access) {
    switch (access) {
    case Access::Read: return kRead;
    case Access::Write: return kWrite;
    case Access::Device: return kDevice;
    }
    return 0;
}

std::string errno_text(const std::string& what) { return what + ": " + std::strerror(errno); }

struct Fd {
    int fd;
    ~Fd() {
        if (fd >= 0) close(fd);
    }
};

} // namespace

SandboxResult apply_sandbox(const std::vector<Rule>& rules) {
    const long version = syscall(SYS_landlock_create_ruleset, nullptr, 0, kCreateRulesetVersion);
    if (version < 1)
        return {0, "this kernel has no Landlock (needs Linux 5.13 or later with the landlock LSM enabled)"};
    const int abi = static_cast<int>(version);

    RulesetAttr attr{};
    attr.handled_access_fs = handled_rights(abi);
    if (abi >= 6) attr.scoped = kScopeAbstractUnixSocket | kScopeSignal;
    // An older kernel rejects a struct larger than the one it knows, so pass only the fields its ABI has.
    const size_t attr_size = abi >= 6 ? sizeof(RulesetAttr) : abi >= 4 ? 2 * sizeof(uint64_t) : sizeof(uint64_t);

    const Fd ruleset{static_cast<int>(syscall(SYS_landlock_create_ruleset, &attr, attr_size, 0))};
    if (ruleset.fd < 0) return {0, errno_text("landlock_create_ruleset")};

    for (const Rule& rule : rules) {
        const Fd target{open(rule.path.c_str(), O_PATH | O_CLOEXEC)};
        if (target.fd < 0) {
            if (!rule.required && (errno == ENOENT || errno == ENOTDIR)) continue;
            return {0, errno_text(rule.path)};
        }
        struct stat st {};
        if (fstat(target.fd, &st) != 0) return {0, errno_text(rule.path)};

        uint64_t rights = rights_of(rule.access) & attr.handled_access_fs;
        if (!S_ISDIR(st.st_mode)) rights &= kFileRights;
        if (rights == 0) continue;

        const PathBeneathAttr beneath{rights, target.fd};
        if (syscall(SYS_landlock_add_rule, ruleset.fd, kRulePathBeneath, &beneath, 0) != 0)
            return {0, errno_text("landlock_add_rule " + rule.path)};
    }

    // Required by landlock_restrict_self, and wanted anyway: no setuid program can raise a child's privileges.
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) return {0, errno_text("PR_SET_NO_NEW_PRIVS")};
    if (syscall(SYS_landlock_restrict_self, ruleset.fd, 0) != 0) return {0, errno_text("landlock_restrict_self")};
    return {abi, {}};
}

} // namespace hxm
