#pragma once

#include <string>
#include <vector>

namespace hx {

enum class Access {
    Read,   // read files, list directories, execute
    Write,  // Read, plus create, change, rename and remove
    Device, // read and write an existing device node, and ioctl on it
};

struct Rule {
    std::string path;
    Access access;
    bool required; // a missing optional path is skipped; a missing required one is an error
};

struct SandboxResult {
    int abi = 0;       // Landlock ABI in force; 0 when the sandbox was not applied
    std::string error; // why not, when abi is 0
};

// Restricts this process, and everything it will ever start, to the given paths with Landlock. Nothing outside
// them can be opened afterwards; file descriptors that are already open keep working. The restriction cannot be
// lifted. Where the kernel supports it, the process is also cut off from signalling processes outside the
// sandbox and from their abstract unix sockets. Network access is not restricted.
//
// Call it while the process is single-threaded: Landlock binds the calling thread and its descendants only.
SandboxResult apply_sandbox(const std::vector<Rule>& rules);

} // namespace hx
