#ifndef HX_SANDBOX_H_
#define HX_SANDBOX_H_

#include <cstdint>
#include <stdexcept>
#include <string>
#include <vector>

namespace hx {

enum class Access : std::uint8_t {
    Read,    // read files, list directories, execute, connect to unix sockets
    Write,   // Read, plus create, change, rename and remove
    Device,  // read and write an existing device node, and ioctl on it
};

struct Rule {
    std::string path;
    Access access;
    bool required;  // an optional path that is missing or out of reach is skipped; for a required one that is an error
};

// The sandbox could not be applied.
class SandboxError : public std::runtime_error {
  public:
    using std::runtime_error::runtime_error;
};

// Proof that the question "is this process confined?" has been answered, one way or the other. It can only be
// obtained from confine() or none(), and a Host cannot be built without one, so no path through the program can
// start running the model's commands having forgotten to ask.
class Sandbox {
  public:
    // Restricts this process, and everything it will ever start, to the given paths with Landlock. Nothing
    // outside them can be opened afterwards; file descriptors that are already open keep working. The
    // restriction cannot be lifted. Where the kernel supports it, the process is also cut off from signalling
    // processes outside the sandbox and from their abstract unix sockets. Network access is not restricted.
    //
    // Below kUnixSocketAbi, Landlock cannot refuse a connection to a unix socket by path. Unless unix_sockets is
    // true, a seccomp filter then refuses to create unix sockets (stream socketpairs still work), on x86-64 and
    // aarch64; elsewhere nothing closes the gap and blocks_unix_sockets() says so.
    //
    // Call it while the process is single-threaded: Landlock binds the calling thread and its descendants only.
    // Throws SandboxError when the kernel cannot do it. The process may then be partly restricted, so the caller
    // should stop rather than carry on without walls.
    [[nodiscard]] static Sandbox confine(const std::vector<Rule>& rules, bool unix_sockets);

    // The user's explicit decision to run without walls.
    [[nodiscard]] static Sandbox none() noexcept { return Sandbox(0, false); }

    // The first Landlock ABI that can refuse connections to unix sockets outside the sandbox.
    static constexpr int kUnixSocketAbi = 9;

    [[nodiscard]] bool confined() const noexcept { return abi_ > 0; }
    // False when confined commands can still connect to unix sockets by path (the D-Bus session bus, ssh-agent).
    [[nodiscard]] bool blocks_unix_sockets() const noexcept { return abi_ >= kUnixSocketAbi || filtered_; }
    // The Landlock ABI in force, or 0 when not confined.
    [[nodiscard]] int abi() const noexcept { return abi_; }

  private:
    explicit Sandbox(int abi, bool filtered) noexcept : abi_(abi), filtered_(filtered) {}

    int abi_;
    bool filtered_;  // the seccomp filter against unix sockets is installed
};

}  // namespace hx

#endif  // HX_SANDBOX_H_
