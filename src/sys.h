#ifndef HX_SYS_H_
#define HX_SYS_H_

#include <sys/syscall.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <ctime>
#include <filesystem>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>

namespace hx {

inline constexpr std::size_t kBufferSize = 65536;

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

struct FreeDeleter {
    // NOLINTNEXTLINE(cppcoreguidelines-no-malloc)
    void operator()(void* memory) const noexcept { std::free(memory); }
};

[[nodiscard]] inline std::string error_text(int code = errno) {
    return std::generic_category().message(code);
}

[[nodiscard]] inline std::string env_or(const char* name, std::string_view fallback = {}) {
    // NOLINTNEXTLINE(concurrency-mt-unsafe)
    const char* value = std::getenv(name);
    return std::string(value != nullptr && *value != '\0' ? value : fallback);
}

[[nodiscard]] inline bool beneath(const std::filesystem::path& path, const std::filesystem::path& dir) {
    const std::filesystem::path inside = path.lexically_relative(dir);
    return !inside.empty() && *inside.begin() != "..";
}

enum class ReadAll : std::uint8_t {
    Done,
    Failed,
    TooLarge,
};

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

inline long cloexec_above_stderr() noexcept {
#ifdef CLOSE_RANGE_CLOEXEC
    return close_range(3, ~0U, CLOSE_RANGE_CLOEXEC);
#else
    constexpr long kCloseRange = 436;
    constexpr unsigned kCloexec = 4;
    return syscall(kCloseRange, 3U, ~0U, kCloexec);
#endif
}

inline timespec timespec_of(double seconds) noexcept {
    if (seconds <= 0) {
        return {.tv_sec = 0, .tv_nsec = 0};
    }
    constexpr long kNanosPerSecond = 1'000'000'000;
    auto whole = static_cast<time_t>(seconds);
    auto nanos = static_cast<long>((seconds - static_cast<double>(whole)) * 1e9);
    if (nanos >= kNanosPerSecond) {
        ++whole;
        nanos -= kNanosPerSecond;
    } else if (nanos < 0) {
        nanos = 0;
    }
    return {.tv_sec = whole, .tv_nsec = nanos};
}

inline void nap(long milliseconds) noexcept {
    if (milliseconds <= 0) {
        return;
    }
    const timespec duration{
        .tv_sec = milliseconds / 1000,
        .tv_nsec = (milliseconds % 1000) * 1'000'000,
    };
    nanosleep(&duration, nullptr);
}

}  // namespace hx

#endif  // HX_SYS_H_
