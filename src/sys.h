#ifndef HX_SYS_H_
#define HX_SYS_H_

#include <unistd.h>

#include <cerrno>
#include <cstdlib>
#include <string>
#include <system_error>
#include <utility>

namespace hx {

// Owns a file descriptor and closes it exactly once.
class UniqueFd {
  public:
    UniqueFd() noexcept = default;
    explicit UniqueFd(int fd) noexcept : fd_(fd) {}
    UniqueFd(UniqueFd&& other) noexcept : fd_(std::exchange(other.fd_, -1)) {}
    UniqueFd& operator=(UniqueFd&& other) noexcept {
        if (this != &other) {
            reset();
            fd_ = std::exchange(other.fd_, -1);
        }
        return *this;
    }
    UniqueFd(const UniqueFd&) = delete;
    UniqueFd& operator=(const UniqueFd&) = delete;
    ~UniqueFd() { reset(); }

    [[nodiscard]] int get() const noexcept { return fd_; }
    [[nodiscard]] bool valid() const noexcept { return fd_ >= 0; }

    void reset() noexcept {
        if (fd_ >= 0) {
            ::close(fd_);
            fd_ = -1;
        }
    }

  private:
    int fd_ = -1;
};

// Frees memory that a C interface returned as malloc'd.
struct FreeDeleter {
    // NOLINTNEXTLINE(cppcoreguidelines-no-malloc): the counterpart of the C interface's malloc
    void operator()(void* memory) const noexcept { std::free(memory); }
};

// The message for an errno value. Unlike strerror, it does not use a shared buffer.
[[nodiscard]] inline std::string error_text(int code = errno) {
    return std::generic_category().message(code);
}

}  // namespace hx

#endif  // HX_SYS_H_
