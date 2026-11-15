#ifndef HX_USAGE_H_
#define HX_USAGE_H_

#include <stdexcept>

namespace hx {

// A problem the user has to fix before hx can start: reported on stderr, exit status 2.
class UsageError : public std::runtime_error {
  public:
    using std::runtime_error::runtime_error;
};

}  // namespace hx

#endif  // HX_USAGE_H_
