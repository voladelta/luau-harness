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

// Before ABI 9, a seccomp filter stands in for kResolveUnix. Reaching a unix socket by path needs a unix socket
// that is not yet tied to a peer, so the filter refuses socket(AF_UNIX, ...). socketpair stays for stream and
// seqpacket pairs, which are connected for good; a datagram pair is refused, because each end can still send to,
// or be connected to, any path. io_uring is refused too, because IORING_OP_SOCKET creates sockets without the
// socket call. A call from another architecture (32-bit code on x86-64 would use socketcall) is refused
// whatever it is.
#if (defined(__x86_64__) || defined(__aarch64__)) && !defined(__ILP32__) && __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__

#ifdef __x86_64__
constexpr uint32_t kAuditArch = AUDIT_ARCH_X86_64;
#else
constexpr uint32_t kAuditArch = AUDIT_ARCH_AARCH64;
#endif
// x32 calls on x86-64 carry this bit in their number and report the x86-64 architecture. No x86-64 or aarch64
// call number comes near it, so one program serves both.
constexpr uint32_t kX32Bit = 0x40000000U;
constexpr uint32_t kIoUringSetup = 425;  // the same on every architecture

constexpr sock_filter stmt(uint32_t code, uint32_t k) {
    return {static_cast<uint16_t>(code), 0, 0, k};
}
constexpr sock_filter jump(uint32_t code, uint32_t k, uint8_t if_true, uint8_t if_false) {
    return {static_cast<uint16_t>(code), if_true, if_false, k};
}

// Returns whether the filter is installed: always, on the architectures it is written for.
bool filter_unix_sockets() {
    constexpr auto kArch = static_cast<uint32_t>(offsetof(seccomp_data, arch));
    constexpr auto kNumber = static_cast<uint32_t>(offsetof(seccomp_data, nr));
    constexpr auto kFirstArg = static_cast<uint32_t>(offsetof(seccomp_data, args));  // its low half: little-endian
    constexpr auto kSecondArg = kFirstArg + static_cast<uint32_t>(sizeof(uint64_t));
    constexpr uint32_t kTypeMask = 0xf;  // the socket type without SOCK_CLOEXEC and SOCK_NONBLOCK
    // A jump skips that many instructions. The last three are the verdicts: 12 allow, 13 ENOSYS, 14 EACCES.
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
    return false;  // not written for this architecture; blocks_unix_sockets() stays false and hx says so
}

#endif

}  // namespace

Sandbox Sandbox::confine(const std::vector<Rule>& rules, bool unix_sockets) {
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
            // An optional path that is missing, or that this user cannot even reach, needs no rule.
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

    // Required by landlock_restrict_self and by a seccomp filter, and wanted anyway: no setuid program can raise
    // a child's privileges.
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) {
        fail("PR_SET_NO_NEW_PRIVS");
    }
    if (syscall(kRestrictSelf, ruleset.get(), 0) != 0) {
        fail("landlock_restrict_self");
    }
    const bool filtered = abi < kUnixSocketAbi && !unix_sockets && filter_unix_sockets();
    return Sandbox(abi, filtered);
}

}  // namespace hx
