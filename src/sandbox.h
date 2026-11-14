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
    bool required;  // a missing optional path is skipped; a missing required one is an error
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
    // Call it while the process is single-threaded: Landlock binds the calling thread and its descendants only.
    // Throws SandboxError, with the process unchanged, when the kernel cannot do it.
    [[nodiscard]] static Sandbox confine(const std::vector<Rule>& rules);

    // The user's explicit decision to run without walls.
    [[nodiscard]] static Sandbox none() noexcept { return Sandbox(0); }

    // The first Landlock ABI that can refuse connections to unix sockets outside the sandbox.
    static constexpr int kUnixSocketAbi = 9;

    [[nodiscard]] bool confined() const noexcept { return abi_ > 0; }
    // False when confined commands can still connect to unix sockets by path (the D-Bus session bus, ssh-agent).
    [[nodiscard]] bool blocks_unix_sockets() const noexcept { return abi_ >= kUnixSocketAbi; }
    // The Landlock ABI in force, or 0 when not confined.
    [[nodiscard]] int abi() const noexcept { return abi_; }

  private:
    explicit Sandbox(int abi) noexcept : abi_(abi) {}

    int abi_;
};

}  // namespace hx

#endif  // HX_SANDBOX_H_
