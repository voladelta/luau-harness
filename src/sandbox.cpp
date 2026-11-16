#include "sandbox.h"

#include <fcntl.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <string_view>
#include <utility>

#include <fmt/format.h>

#include "sys.h"

namespace hx {
namespace {

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
constexpr uint64_t kRefer = 1ULL << 13;
constexpr uint64_t kTruncate = 1ULL << 14;
constexpr uint64_t kIoctlDev = 1ULL << 15;
constexpr uint64_t kResolveUnix = 1ULL << 16;

constexpr uint64_t kScopeAbstractUnixSocket = 1ULL << 0;
constexpr uint64_t kScopeSignal = 1ULL << 1;

constexpr uint32_t kCreateRulesetVersion = 1U << 0;
constexpr int kRulePathBeneath = 1;

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
    uint64_t handled_access_net;
    uint64_t scoped;
};

struct [[gnu::packed]] PathBeneathAttr {
    uint64_t allowed_access;
    int32_t parent_fd;
};

constexpr uint64_t kRead = kExecute | kReadFile | kReadDir | kResolveUnix;
constexpr uint64_t kWrite = kRead | kWriteFile | kRemoveDir | kRemoveFile | kMakeDir | kMakeReg | kMakeSock |
                            kMakeFifo | kMakeSym | kRefer | kTruncate;
constexpr uint64_t kDevice = kReadFile | kReadDir | kWriteFile | kIoctlDev;
constexpr uint64_t kFileRights = kExecute | kWriteFile | kReadFile | kTruncate | kIoctlDev | kResolveUnix;

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

[[noreturn]] void fail(std::string_view what) {
    throw SandboxError(fmt::format("{}: {}", what, error_text()));
}

#if (defined(__x86_64__) || defined(__aarch64__)) && !defined(__ILP32__) && __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__

#ifdef __x86_64__
constexpr uint32_t kAuditArch = AUDIT_ARCH_X86_64;
#else
constexpr uint32_t kAuditArch = AUDIT_ARCH_AARCH64;
#endif
constexpr uint32_t kX32Bit = 0x40000000U;
constexpr uint32_t kIoUringSetup = 425;

constexpr sock_filter stmt(uint32_t code, uint32_t k) {
    return {static_cast<uint16_t>(code), 0, 0, k};
}
constexpr sock_filter jump(uint32_t code, uint32_t k, uint8_t if_true, uint8_t if_false) {
    return {static_cast<uint16_t>(code), if_true, if_false, k};
}

bool filter_unix_sockets() {
    constexpr auto kArch = static_cast<uint32_t>(offsetof(seccomp_data, arch));
    constexpr auto kNumber = static_cast<uint32_t>(offsetof(seccomp_data, nr));
    constexpr auto kFirstArg = static_cast<uint32_t>(offsetof(seccomp_data, args));
    constexpr auto kSecondArg = kFirstArg + static_cast<uint32_t>(sizeof(uint64_t));
    constexpr uint32_t kTypeMask = 0xf;
    std::array<sock_filter, 15> program{{
        stmt(BPF_LD | BPF_W | BPF_ABS, kArch),
        jump(BPF_JMP | BPF_JEQ | BPF_K, kAuditArch, 0, 11),
        stmt(BPF_LD | BPF_W | BPF_ABS, kNumber),
        jump(BPF_JMP | BPF_JGE | BPF_K, kX32Bit, 9, 0),
        jump(BPF_JMP | BPF_JEQ | BPF_K, kIoUringSetup, 8, 0),
        jump(BPF_JMP | BPF_JEQ | BPF_K, SYS_socket, 0, 2),
        stmt(BPF_LD | BPF_W | BPF_ABS, kFirstArg),
        jump(BPF_JMP | BPF_JEQ | BPF_K, AF_UNIX, 6, 4),
        jump(BPF_JMP | BPF_JEQ | BPF_K, SYS_socketpair, 0, 3),
        stmt(BPF_LD | BPF_W | BPF_ABS, kSecondArg),
        stmt(BPF_ALU | BPF_AND | BPF_K, kTypeMask),
        jump(BPF_JMP | BPF_JEQ | BPF_K, SOCK_DGRAM, 2, 0),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOSYS),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EACCES),
    }};
    const sock_fprog fprog{static_cast<unsigned short>(program.size()), program.data()};
    if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &fprog, 0, 0) != 0) {
        fail("PR_SET_SECCOMP");
    }
    return true;
}

#else

bool filter_unix_sockets() {
    return false;
}

#endif

std::pair<UniqueFd, int> make_ruleset(const std::vector<Rule>& rules) {
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
    size_t attr_size = sizeof(uint64_t);
    if (abi >= 6) {
        attr_size = sizeof(RulesetAttr);
    } else if (abi >= 4) {
        attr_size = 2 * sizeof(uint64_t);
    }

    UniqueFd ruleset(static_cast<int>(syscall(kCreateRuleset, &attr, attr_size, 0)));
    if (!ruleset.valid()) {
        fail("landlock_create_ruleset");
    }

    for (const Rule& rule : rules) {
        const UniqueFd target(open(rule.path.c_str(), O_PATH | O_CLOEXEC));
        if (!target.valid()) {
            if (!rule.required && (errno == ENOENT || errno == ENOTDIR || errno == EACCES)) {
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
    return {std::move(ruleset), abi};
}

bool restrict_self(int ruleset) noexcept {
    return prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0 && syscall(kRestrictSelf, ruleset, 0) == 0;
}

}  // namespace

Sandbox Sandbox::confine(const std::vector<Rule>& rules, bool unix_sockets) {
    const auto [ruleset, abi] = make_ruleset(rules);
    if (!restrict_self(ruleset.get())) {
        fail("landlock_restrict_self");
    }
    const bool filtered = abi < kUnixSocketAbi && !unix_sockets && filter_unix_sockets();
    return Sandbox(abi, filtered);
}

Wall Wall::prepare(const std::vector<Rule>& rules) {
    return Wall(make_ruleset(rules).first);
}

bool Wall::enter() const noexcept {
    return restrict_self(ruleset_.get());
}

}  // namespace hx
