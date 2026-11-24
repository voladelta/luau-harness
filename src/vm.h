#ifndef HX_VM_H_
#define HX_VM_H_

#include <functional>
#include <string>
#include <string_view>

#include "host.h"
#include "sandbox.h"

namespace hx {

inline constexpr const char* kProjectLoop = ".hx/loop.luau";

int run_module(HostConfig config, const Sandbox& sandbox, std::string_view source, const std::string& name,
               const std::function<void()>& on_done = {});

int run_loop(HostConfig config, const Sandbox& sandbox, bool built_in, const std::function<void()>& on_done = {});

}  // namespace hx

#endif  // HX_VM_H_
