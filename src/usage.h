#ifndef HX_USAGE_H_
#define HX_USAGE_H_

#include <stdexcept>

namespace hx {

inline constexpr int kExitFailure = 1;  // the run failed
inline constexpr int kExitUsage = 2;    // bad usage, or hx cannot start here

// A problem the user has to fix before hx can start: reported on stderr, exit status kExitUsage.
class UsageError : public std::runtime_error {
  public:
    using std::runtime_error::runtime_error;
};

}  // namespace hx

#endif  // HX_USAGE_H_
