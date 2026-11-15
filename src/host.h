#ifndef HX_HOST_H_
#define HX_HOST_H_

#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "sandbox.h"
#include "sys.h"

struct lua_State;
using CURL = void;  // as curl.h has it, so this header need not include curl.h

namespace hx {

// How an endpoint is given its key.
enum class Auth : std::uint8_t {
    ApiKey,  // x-api-key: KEY
    Bearer,  // Authorization: Bearer KEY
};

// A URL that host.llm or host.tool posts to, with the key that goes to it and nowhere else. Luau code never sees a
// key.
struct Endpoint {
    std::string name;                  // "llm" for host.llm; for host.tool, the name Luau asks for
    std::string url;                   // posted to as it is; a redirect is not followed
    Auth auth = Auth::ApiKey;          // how the key is sent
    std::string key;                   // empty for none, and then no key header is sent
    std::string key_env;               // where the key comes from; when set, a request without a key is refused
    std::vector<std::string> headers;  // more request headers, each "Name: value"
    bool form = false;                 // a table request is sent as a multipart form; otherwise JSON text only
};

struct HostConfig {
    std::vector<Endpoint> endpoints;  // what host.llm and host.tool reach, by name
    std::string model;                // host.model: the model host.llm's requests are meant for
    std::string ca_bundle;            // a file of trust roots to use instead of libcurl's own; empty for those
    std::vector<std::string> args;    // host.args
    bool interactive = false;         // host.interactive: stdin is a terminal and no task was given
    bool quiet = false;               // host.quiet: --quiet, so the loop leaves out progress lines
    std::string default_loop;         // host.default_loop: the built-in loop's source
    std::vector<std::pair<std::string, std::string>> extensions;  // host.extensions: built-in ones, name and source
    std::vector<Rule> system_rules;  // what every wall lets in: system directories, harmless devices, trust roots
    UniqueFd judge;                  // a line-based channel to the judge process (--judge); invalid for none
};

// The machine as Luau sees it. open() registers the global `host` table, which is the whole interface between
// the VM and the system:
//
//   host.llm(body, {timeout=600, beta=}) -> status, text | nil, error     beta: the anthropic-beta header
//   host.tool(name, request, {timeout=300}) -> status, text | nil, error    DeepSeek's search, fetch or ocr, or
//                                                     Exa's search for "exa"; request is JSON text, or (DeepSeek
//                                                     only) a form table whose `file` is a path. Each name is an
//                                                     endpoint in HostConfig::endpoints, as host.llm's is "llm"
//   host.sh(command, {timeout=120, read=, write=}) -> {out=, code=, signal=, timed_out=, seconds=}
//                                                     read and write are lists of paths: given either, the command
//                                                     runs behind a narrower wall, the system directories and those,
//                                                     and what it leaves running is killed. seconds: fork to exit.
//                                                     hangup=true: once stdin's writer hangs up, the command ends
//                                                     (or does not start) and host.sh raises "hung up"
//   host.judge(line?) -> {answers}, pending           sends a line to the judge, if given, and waits for what it
//                                                     owes, until 2 s before the host.timed limit; a line is
//                                                     refused while an answer is owed
//   host.read(path) -> string | nil, error
//   host.write(path, data) -> true | nil, error       creates missing parent directories; replaces a regular file
//                                                     by renaming a new one with its mode over it, so it is never
//                                                     left cut short; writes in place where a new file cannot
//                                                     stand in for it (hard links, a device, a dangling link)
//   host.list(dir) -> {names} | nil, error            sorted; directories end in "/"
//   host.load(source, name?, env?) -> function | nil, error
//   host.try(fn, ...) -> true, ... | false, error     pcall that is also where a stop ends (below)
//   host.timed(seconds, fn, ...) -> true, ... | false, error     host.try with a time limit ("timeout");
//                                                     llm, sh and sleep inside it end when the limit does
//   host.input(prompt?) -> line | nil at end of input
//   host.log(text)                                    one line on stderr
//   host.sleep(seconds), host.env(name)
//   host.args, host.cwd, host.interactive, host.quiet, host.sandboxed, host.default_loop
//   host.model                                        the model name host.llm's requests are meant for
//   host.tty                                          stdout is a terminal
//   host.tools                                        a DeepSeek key is there for host.tool
//   host.exa                                          an Exa key is there for host.tool("exa")
//   host.extensions                                   the built-in extensions' sources, by name
//   host.judged                                       a judge is there for host.judge
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
    static int judge(lua_State* L);

    // Shortens a wait so that it ends no later than the host.timed limit in force. A negative wait is an empty one.
    // The next VM safepoint reads the clock, so a limit that the wait ran into is raised as soon as Luau code runs.
    [[nodiscard]] double within_limit(double seconds);
    // The narrower wall host.sh's options at idx ask for, prepared before the fork.
    [[nodiscard]] std::optional<Wall> narrower_wall(lua_State* L, int idx) const;
    // The endpoint of that name, or null.
    [[nodiscard]] const Endpoint* endpoint(std::string_view name) const;
    // The HTTP handle every request uses, reset for each, so that a connection is kept from one request to the
    // next instead of being set up again. Null when libcurl cannot make one.
    CURL* curl();

    struct CurlDeleter {
        void operator()(CURL* handle) const noexcept;
    };

    HostConfig config_;
    bool sandboxed_;
    // Made at the first request.
    std::unique_ptr<CURL, CurlDeleter> curl_;
    double deadline_ = 0;   // the innermost host.timed limit on the monotonic clock; 0 for none
    int countdown_ = 1;     // VM safepoints left until the clock is next read against deadline_
    int stride_ = 1;        // safepoints from one clock read to the next, so that one comes about every millisecond
    double checked_ = 0;    // when the clock was last read against deadline_
    int tries_ = 0;         // host.try and host.timed calls in progress
    std::string judge_in_;  // what the judge sent that is not yet a whole line
    int judge_owed_ = 0;    // answers the judge still owes: a later host.judge returns them
};

// Makes SIGINT, SIGTERM and SIGHUP stop the running command or request and surface as a Luau error. A second
// signal that arrives before the first was handled kills every descendant, as ~Host would, and exits at once.
void install_signal_handlers();

}  // namespace hx

#endif  // HX_HOST_H_
