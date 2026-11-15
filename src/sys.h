#ifndef HX_SYS_H_
#define HX_SYS_H_

// Small wrappers around the system: owned descriptors, whole reads and writes, the environment.

#include <sys/syscall.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <ctime>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>

namespace hx {

// The size of the buffer a read from a file, a pipe or a socket goes through.
inline constexpr std::size_t kBufferSize = 65536;

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

// The value of an environment variable, or fallback when it is unset or empty.
[[nodiscard]] inline std::string env_or(const char* name, std::string_view fallback = {}) {
    // NOLINTNEXTLINE(concurrency-mt-unsafe): the environment is only changed while hx has one thread
    const char* value = std::getenv(name);
    return std::string(value != nullptr && *value != '\0' ? value : fallback);
}

enum class ReadAll : std::uint8_t {
    Done,
    Failed,    // errno says why
    TooLarge,  // there was more than the limit
};

// Reads fd to its end and appends what it holds to data, going on after a signal.
[[nodiscard]] inline ReadAll read_all(int fd, std::string& data, std::size_t limit = std::string::npos) {
    std::array<char, kBufferSize> buffer{};
    for (;;) {
        const ssize_t n = ::read(fd, buffer.data(), buffer.size());
        if (n == 0) {
            return ReadAll::Done;
        }
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return ReadAll::Failed;
        }
        if (data.size() + static_cast<std::size_t>(n) > limit) {
            return ReadAll::TooLarge;
        }
        data.append(buffer.data(), static_cast<std::size_t>(n));
    }
}

// Writes all of data, going on after a signal. Returns false with errno set when the system refuses.
[[nodiscard]] inline bool write_all(int fd, std::string_view data) {
    while (!data.empty()) {
        const ssize_t n = ::write(fd, data.data(), data.size());
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return false;
        }
        data.remove_prefix(static_cast<std::size_t>(n));
    }
    return true;
}

// Marks every descriptor above stderr close-on-exec, so a program this process starts next inherits none of them.
// A system call only, so a forked child may call it. Returns 0, or -1 with errno set.
inline long cloexec_above_stderr() noexcept {
#ifdef CLOSE_RANGE_CLOEXEC  // glibc 2.34 declares the wrapper and the flag together
    return close_range(3, ~0U, CLOSE_RANGE_CLOEXEC);
#else  // the system call is Linux 5.11, the same number on every architecture
    constexpr long kCloseRange = 436;
    constexpr unsigned kCloexec = 4;  // CLOSE_RANGE_CLOEXEC
    return syscall(kCloseRange, 3U, ~0U, kCloexec);
#endif
}

// Sleeps for a moment, between looks at something that cannot be waited for. A system call only, so a
// signal handler may call it; a signal cuts it short.
inline void nap(long milliseconds) noexcept {
    const timespec duration{0, milliseconds * 1'000'000};
    nanosleep(&duration, nullptr);
}

}  // namespace hx

#endif  // HX_SYS_H_
