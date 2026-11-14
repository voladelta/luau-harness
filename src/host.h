#ifndef HX_HOST_H_
#define HX_HOST_H_

#include <sys/types.h>

#include <string>
#include <vector>

#include "sandbox.h"

struct lua_State;

namespace hx {

struct HostConfig {
    std::string api_url;            // the one endpoint host.llm posts to
    std::string api_key;            // sent as x-api-key; Luau code never sees it
    std::string ca_bundle;          // PEM text of extra trust roots, read before the sandbox closed the file off
    std::vector<std::string> args;  // host.args
    bool interactive = false;       // host.interactive: stdin is a terminal and no task was given
    std::string default_loop;       // host.default_loop: the built-in loop's source
};

// The machine as Luau sees it. open() registers the global `host` table, which is the whole interface between
// the VM and the system:
//
//   host.llm(body, {timeout=600}) -> status, text | nil, error
//   host.sh(command, {timeout=120}) -> {out=, code=, signal=, timed_out=}
//   host.read(path) -> string | nil, error
//   host.write(path, data) -> true | nil, error       creates missing parent directories
//   host.list(dir) -> {names} | nil, error            sorted; directories end in "/"
//   host.load(source, name?, env?) -> function | nil, error
//   host.timed(seconds, fn, ...) -> true, ... | false, error     pcall with a time limit ("timeout");
//                                                     llm, sh and sleep inside it end when the limit does
//   host.input(prompt?) -> line | nil at end of input
//   host.log(text)                                    one line on stderr
//   host.sleep(seconds), host.env(name)
//   host.args, host.cwd, host.interactive, host.sandboxed, host.default_loop
//
// A signal becomes exactly one Luau error, raised by whichever notices it first: the blocking call it cut
// short (llm, sh, sleep, input) or the next VM safepoint. SIGINT raises "interrupted", SIGTERM and SIGHUP raise
// "terminated". An expired host.timed limit raises "timeout" the same way.
//
// Destroying the Host kills every process group host.sh started that is still alive.
class Host {
  public:
    // The Sandbox is required as evidence that confinement was decided before anything can run.
    Host(HostConfig config, const Sandbox& sandbox);
    Host(const Host&) = delete;
    Host& operator=(const Host&) = delete;
    Host(Host&&) = delete;
    Host& operator=(Host&&) = delete;
    ~Host();

    // Attaches this host to a VM. The Host must outlive the lua_State.
    void open(lua_State* L);

  private:
    static Host& of(lua_State* L);
    static void on_interrupt(lua_State* L, int gc);
    static int llm(lua_State* L);
    static int sh(lua_State* L);
    static int timed(lua_State* L);
    static int sleep(lua_State* L);

    // Shortens a wait so that it ends no later than the host.timed limit in force.
    [[nodiscard]] double within_limit(double seconds) const;

    HostConfig config_;
    bool sandboxed_;
    double deadline_ = 0;        // the innermost host.timed limit on the monotonic clock; 0 for none
    std::vector<pid_t> groups_;  // process groups host.sh started
};

// Makes SIGINT, SIGTERM and SIGHUP stop the running command or request and surface as a Luau error. A second
// signal that arrives before the first was handled exits the process at once.
void install_signal_handlers();

}  // namespace hx

#endif  // HX_HOST_H_
