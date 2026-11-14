#pragma once

#include <string>
#include <vector>

struct lua_State;

namespace hxm {

struct HostConfig {
    std::string api_url;             // the one endpoint host.llm posts to
    std::string api_key;             // sent as a bearer token; Luau code never sees it
    std::string ca_bundle;           // PEM text of extra trust roots, read before the sandbox closed the file off
    std::vector<std::string> args;   // host.args
    bool interactive = false;        // host.interactive: stdin is a terminal and no task was given
    std::string default_loop;        // host.default_loop: the built-in loop's source
};

// Registers the global `host` table, the whole interface between Luau and the machine:
//
//   host.llm(body, {timeout=600}) -> status, text | nil, error
//   host.sh(command, {timeout=120}) -> {out=, code=, signal=, timed_out=}
//   host.read(path) -> string | nil, error
//   host.write(path, data) -> true | nil, error       creates missing parent directories
//   host.list(dir) -> {names} | nil, error            sorted; directories end in "/"
//   host.load(source, name?, env?) -> function | nil, error
//   host.timed(seconds, fn, ...) -> true, ... | false, error     pcall with a time limit ("timeout")
//   host.input(prompt?) -> line | nil at end of input
//   host.log(text)                                    one line on stderr
//   host.sleep(seconds), host.env(name)
//   host.args, host.cwd, host.interactive, host.default_loop
//
// It also installs the VM interrupt that turns SIGINT into the Luau error "interrupted", SIGTERM and SIGHUP
// into "terminated", and an expired host.timed limit into "timeout". The config must outlive the state.
void open_host(lua_State* L, const HostConfig& config);

// Makes SIGINT, SIGTERM and SIGHUP stop the running command or request and surface as a Luau error. A second
// signal that arrives before the first was handled exits the process at once.
void install_signal_handlers();

// Kills every process group host.sh started that is still alive. Call before exiting.
void kill_children();

} // namespace hxm
