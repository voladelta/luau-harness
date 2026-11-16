#ifndef HX_USAGE_H_
#define HX_USAGE_H_

#include <stdexcept>

namespace hx {

inline constexpr int kExitFailure = 1;
inline constexpr int kExitUsage = 2;

class UsageError : public std::runtime_error {
  public:
    using std::runtime_error::runtime_error;
};

}  // namespace hx

#endif  // HX_USAGE_H_
