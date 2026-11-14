#include "sandbox.h"

#include <fcntl.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <string_view>

#include <fmt/format.h>

#include "sys.h"

namespace hx {
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
constexpr uint64_t kRefer = 1ULL << 13;     // ABI 2
constexpr uint64_t kTruncate = 1ULL << 14;  // ABI 3
constexpr uint64_t kIoctlDev = 1ULL << 15;  // ABI 5
// Connecting to a unix socket by path, when the socket was created outside the sandbox. Before ABI 9 Landlock
// cannot refuse this, which leaves the D-Bus session bus, ssh-agent and the like within reach.
constexpr uint64_t kResolveUnix = 1ULL << 16;  // ABI 9

constexpr uint64_t kScopeAbstractUnixSocket = 1ULL << 0;  // ABI 6
constexpr uint64_t kScopeSignal = 1ULL << 1;              // ABI 6

constexpr uint32_t kCreateRulesetVersion = 1U << 0;
constexpr int kRulePathBeneath = 1;

// Syscall numbers, the same on every architecture; taken from the system headers when they have them.
#ifdef SYS_landlock_create_ruleset
constexpr long kCreateRuleset = SYS_landlock_create_ruleset;
constexpr long kAddRule = SYS_landlock_add_rule;
constexpr long kRestrictSelf = SYS_landlock_restrict_self;
#else
constexpr long kCreateRuleset = 444;
constexpr long kAddRule = 445;
constexpr long kRestrictSelf = 446;
#endif

struct RulesetAttr {
    uint64_t handled_access_fs;
    uint64_t handled_access_net;  // ABI 4; left at zero: the network stays open
    uint64_t scoped;              // ABI 6
};

struct [[gnu::packed]] PathBeneathAttr {
    uint64_t allowed_access;
    int32_t parent_fd;
};

constexpr uint64_t kRead = kExecute | kReadFile | kReadDir | kResolveUnix;
constexpr uint64_t kWrite = kRead | kWriteFile | kRemoveDir | kRemoveFile | kMakeDir | kMakeReg | kMakeSock |
                            kMakeFifo | kMakeSym | kRefer | kTruncate;
constexpr uint64_t kDevice = kReadFile | kReadDir | kWriteFile | kIoctlDev;
// The rights that mean something on a file; the kernel rejects a rule that puts any other on one.
constexpr uint64_t kFileRights = kExecute | kWriteFile | kReadFile | kTruncate | kIoctlDev | kResolveUnix;

// Every right this ABI knows. All of them are handled, so anything not granted by a rule is denied; device and
// block nodes can never be created because no rule grants kMakeChar or kMakeBlock.
uint64_t handled_rights(int abi) {
    uint64_t rights = kExecute | kReadFile | kReadDir | kWriteFile | kRemoveDir | kRemoveFile | kMakeChar | kMakeDir |
                      kMakeReg | kMakeSock | kMakeFifo | kMakeBlock | kMakeSym;
    if (abi >= 2) {
        rights |= kRefer;
    }
    if (abi >= 3) {
        rights |= kTruncate;
    }
    if (abi >= 5) {
        rights |= kIoctlDev;
    }
    if (abi >= Sandbox::kUnixSocketAbi) {
        rights |= kResolveUnix;
    }
    return rights;
}

uint64_t rights_of(Access access) {
    switch (access) {
        case Access::Read:
            return kRead;
        case Access::Write:
            return kWrite;
        case Access::Device:
            return kDevice;
    }
    return 0;
}

// Reports a system call that just failed as "what: reason".
[[noreturn]] void fail(std::string_view what) {
    throw SandboxError(fmt::format("{}: {}", what, error_text()));
}

}  // namespace

Sandbox Sandbox::confine(const std::vector<Rule>& rules) {
    const long version = syscall(kCreateRuleset, nullptr, 0, kCreateRulesetVersion);
    if (version < 1) {
        throw SandboxError("this kernel has no Landlock (needs Linux 5.13 or later with the landlock LSM enabled)");
    }
    const int abi = static_cast<int>(version);

    RulesetAttr attr{};
    attr.handled_access_fs = handled_rights(abi);
    if (abi >= 6) {
        attr.scoped = kScopeAbstractUnixSocket | kScopeSignal;
    }
    // An older kernel rejects a struct larger than the one it knows, so pass only the fields its ABI has.
    size_t attr_size = sizeof(uint64_t);
    if (abi >= 6) {
        attr_size = sizeof(RulesetAttr);
    } else if (abi >= 4) {
        attr_size = 2 * sizeof(uint64_t);
    }

    const UniqueFd ruleset(static_cast<int>(syscall(kCreateRuleset, &attr, attr_size, 0)));
    if (!ruleset.valid()) {
        fail("landlock_create_ruleset");
    }

    for (const Rule& rule : rules) {
        const UniqueFd target(open(rule.path.c_str(), O_PATH | O_CLOEXEC));
        if (!target.valid()) {
            if (!rule.required && (errno == ENOENT || errno == ENOTDIR)) {
                continue;
            }
            fail(rule.path);
        }
        struct stat st {};
        if (fstat(target.get(), &st) != 0) {
            fail(rule.path);
        }

        uint64_t rights = rights_of(rule.access) & attr.handled_access_fs;
        if (!S_ISDIR(st.st_mode)) {
            rights &= kFileRights;
        }
        if (rights == 0) {
            continue;
        }

        const PathBeneathAttr beneath{rights, target.get()};
        if (syscall(kAddRule, ruleset.get(), kRulePathBeneath, &beneath, 0) != 0) {
            fail(fmt::format("landlock_add_rule {}", rule.path));
        }
    }

    // Required by landlock_restrict_self, and wanted anyway: no setuid program can raise a child's privileges.
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) {
        fail("PR_SET_NO_NEW_PRIVS");
    }
    if (syscall(kRestrictSelf, ruleset.get(), 0) != 0) {
        fail("landlock_restrict_self");
    }
    return Sandbox(abi);
}

}  // namespace hx
