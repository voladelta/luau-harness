#ifndef HX_HOST_H_
#define HX_HOST_H_

#include <string>
#include <utility>
#include <vector>

#include "sandbox.h"

struct lua_State;

namespace hx {

struct HostConfig {
    std::string api_url;            // the one endpoint host.llm posts to
    std::string api_key;            // sent as x-api-key; Luau code never sees it
    std::string tools_url;          // where host.tool posts: NAME is appended as /NAME
    std::string tools_key;          // DEEPSEEK_API_KEY, sent as a bearer token by host.tool; empty for none
    std::string exa_url;            // where host.tool("exa") posts: Exa's /search
    std::string exa_key;            // EXA_API_KEY, sent as x-api-key by host.tool("exa"); empty for none
    std::string ca_bundle;          // a file of trust roots to use instead of libcurl's own; empty for those
    std::vector<std::string> args;  // host.args
    bool interactive = false;       // host.interactive: stdin is a terminal and no task was given
    bool quiet = false;             // host.quiet: --quiet, so the loop leaves out progress lines
    std::string default_loop;       // host.default_loop: the built-in loop's source
    std::vector<std::pair<std::string, std::string>> extensions;  // host.extensions: built-in ones, name and source
};

// The machine as Luau sees it. open() registers the global `host` table, which is the whole interface between
// the VM and the system:
//
//   host.llm(body, {timeout=600, beta=}) -> status, text | nil, error     beta: the anthropic-beta header
//   host.tool(name, request, {timeout=300}) -> status, text | nil, error    DeepSeek's search, fetch or ocr, or
//                                                     Exa's search for "exa"; request is JSON text, or (DeepSeek
//                                                     only) a form table whose `file` is a path
//   host.sh(command, {timeout=120}) -> {out=, code=, signal=, timed_out=}
//   host.read(path) -> string | nil, error
//   host.write(path, data) -> true | nil, error       creates missing parent directories; replaces a file by
//                                                     renaming a new one over it, so it is never left cut short
//   host.list(dir) -> {names} | nil, error            sorted; directories end in "/"
//   host.load(source, name?, env?) -> function | nil, error
//   host.try(fn, ...) -> true, ... | false, error     pcall that is also where a stop ends (below)
//   host.timed(seconds, fn, ...) -> true, ... | false, error     host.try with a time limit ("timeout");
//                                                     llm, sh and sleep inside it end when the limit does
//   host.input(prompt?) -> line | nil at end of input
//   host.log(text)                                    one line on stderr
//   host.sleep(seconds), host.env(name)
//   host.args, host.cwd, host.interactive, host.quiet, host.sandboxed, host.default_loop
//   host.tty                                          stdout is a terminal
//   host.tools                                        a DeepSeek key is there for host.tool
//   host.exa                                          an Exa key is there for host.tool("exa")
//   host.extensions                                   the built-in extensions' sources, by name
//
// A signal starts a stop, which behaves like a time limit that has already passed. SIGINT raises "interrupted",
// SIGTERM and SIGHUP raise "terminated", and every VM safepoint and every blocking call (llm, sh, sleep, input)
// raises it again for as long as the stop lasts; a blocking call that starts then does not start its work. The
// stop ends only when the outermost host.try or host.timed returns: that call returns false and the stop's error,
// whatever its function did. Inner ones return the same and leave the stop in progress. Outside all of them it
// unwinds to the top, so no pcall can swallow it. An expired host.timed limit raises "timeout" the same way, until
// its call returns.
//
// The Host makes hx a child subreaper, so whatever a command leaves running stays a descendant of hx, even after
// setsid. Destroying the Host kills every descendant.
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
    static int tool(lua_State* L);
    static int sh(lua_State* L);
    static int try_(lua_State* L);
    static int timed(lua_State* L);
    static int protect(lua_State* L, int fn, double deadline);
    static int sleep(lua_State* L);

    // Shortens a wait so that it ends no later than the host.timed limit in force. A negative wait is an empty one.
    [[nodiscard]] double within_limit(double seconds) const;

    HostConfig config_;
    bool sandboxed_;
    double deadline_ = 0;  // the innermost host.timed limit on the monotonic clock; 0 for none
    int tries_ = 0;        // host.try and host.timed calls in progress
};

// Makes SIGINT, SIGTERM and SIGHUP stop the running command or request and surface as a Luau error. A second
// signal that arrives before the first was handled kills every descendant, as ~Host would, and exits at once.
void install_signal_handlers();

}  // namespace hx

#endif  // HX_HOST_H_
