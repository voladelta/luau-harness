// host.llm and host.tool: HTTP requests to the configured endpoints, each with its own key and nowhere else.

#include <curl/curl.h>
#include <fcntl.h>
#include <sys/stat.h>

#include <algorithm>
#include <array>
#include <cctype>
#include <cstddef>
#include <initializer_list>
#include <memory>
#include <new>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <fmt/format.h>

#include "host.h"
#include "host_internal.h"
#include "lua.h"
#include "lualib.h"
#include "sys.h"

namespace hx {
namespace {

constexpr std::size_t kMaxResponse = std::size_t{256} << 20;  // host.llm and host.tool abort a larger reply
constexpr std::size_t kMaxUpload = std::size_t{100} << 20;    // the largest file DeepSeek's OCR takes

std::size_t on_body(char* data, std::size_t size, std::size_t count, void* user) noexcept {
    auto* body = static_cast<std::string*>(user);
    const std::size_t bytes = size * count;
    if (body->size() + bytes > kMaxResponse) {
        return 0;  // a short count aborts the transfer
    }
    try {
        body->append(data, bytes);
    } catch (...) {
        return 0;
    }
    return bytes;
}

int on_progress(void* /*user*/, curl_off_t /*dltotal*/, curl_off_t /*dlnow*/, curl_off_t /*ultotal*/,
                curl_off_t /*ulnow*/) noexcept {
    return g_signal != 0 ? 1 : 0;  // non-zero aborts the transfer
}

// An owned list of request headers.
class Headers {
  public:
    Headers() = default;
    Headers(const Headers&) = delete;
    Headers& operator=(const Headers&) = delete;
    ~Headers() { curl_slist_free_all(list_); }

    void add(const std::string& header) {
        curl_slist* grown = curl_slist_append(list_, header.c_str());
        if (grown == nullptr) {
            throw std::bad_alloc();
        }
        list_ = grown;
    }

    [[nodiscard]] curl_slist* get() const noexcept { return list_; }

  private:
    curl_slist* list_ = nullptr;
};

struct CurlMimeDeleter {
    void operator()(curl_mime* mime) const noexcept { curl_mime_free(mime); }
};
using CurlMime = std::unique_ptr<curl_mime, CurlMimeDeleter>;

// The `beta` field of host.llm's options: the anthropic-beta header, a comma-separated list of beta names. Only
// letters, digits, '-', '_' and ',' are taken, so the value cannot end the header and start another.
std::string beta_option(lua_State* L, int idx) {
    if (lua_isnoneornil(L, idx)) {
        return {};
    }
    luaL_checktype(L, idx, LUA_TTABLE);
    lua_getfield(L, idx, "beta");
    std::string value;
    if (!lua_isnil(L, -1)) {
        std::size_t length = 0;
        const char* text = luaL_checklstring(L, -1, &length);
        value.assign(text, length);
    }
    lua_pop(L, 1);
    for (const char c : value) {
        if (std::isalnum(static_cast<unsigned char>(c)) == 0 && c != '-' && c != '_' && c != ',') {
            luaL_error(L, "host.llm: beta is beta names separated by commas, not %s", value.c_str());
        }
    }
    return value;
}

// Sends a POST whose body is already set on curl to endpoint, with its key, its headers and `extra`, and returns
// status, text, and the seconds of a Retry-After header if there was one | nil, error. Redirects are not followed: a
// key goes to the URL it was meant for and nowhere else. The handle is reset afterwards, so it keeps no pointer into
// this request; the connection stays open for the next.
int post(lua_State* L, CURL* curl, const Endpoint& endpoint, std::initializer_list<std::string_view> extra,
         double timeout, const std::string& ca_bundle) {
    Headers headers;
    if (!endpoint.key.empty()) {
        headers.add(endpoint.auth == Auth::Bearer ? fmt::format("Authorization: Bearer {}", endpoint.key)
                                                  : fmt::format("x-api-key: {}", endpoint.key));
    }
    for (const std::string& header : endpoint.headers) {
        headers.add(header);
    }
    for (const std::string_view header : extra) {
        if (!header.empty()) {
            headers.add(std::string(header));
        }
    }
    std::string response;
    std::array<char, CURL_ERROR_SIZE> error_buffer{};
    curl_easy_setopt(curl, CURLOPT_URL, endpoint.url.c_str());
    curl_easy_setopt(curl, CURLOPT_PROTOCOLS_STR, "http,https");
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers.get());
    // A model can take minutes to answer a non-streaming request, with nothing on the wire meanwhile; probes keep
    // a NAT or a proxy on the way from dropping the connection as idle.
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPALIVE, 1L);
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPIDLE, 60L);
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPINTVL, 30L);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, on_body);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA, &response);
    curl_easy_setopt(curl, CURLOPT_XFERINFOFUNCTION, on_progress);
    curl_easy_setopt(curl, CURLOPT_NOPROGRESS, 0L);
    curl_easy_setopt(curl, CURLOPT_NOSIGNAL, 1L);
    curl_easy_setopt(curl, CURLOPT_CONNECTTIMEOUT, 30L);
    // Zero would mean "no limit" to libcurl, so an exhausted limit becomes the shortest one.
    curl_easy_setopt(curl, CURLOPT_TIMEOUT_MS, std::max(1L, static_cast<long>(timeout * 1000)));
    curl_easy_setopt(curl, CURLOPT_ERRORBUFFER, error_buffer.data());
    if (!ca_bundle.empty()) {
        curl_easy_setopt(curl, CURLOPT_CAINFO, ca_bundle.c_str());
    }

    const CURLcode code = curl_easy_perform(curl);
    long status = 0;
    curl_off_t retry_after = 0;  // seconds the server asked for in Retry-After; 0 when it sent none
    curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &status);
    curl_easy_getinfo(curl, CURLINFO_RETRY_AFTER, &retry_after);
    curl_easy_reset(curl);
    if (code != CURLE_OK) {
        raise_if_signaled(L);
        return fail(L, error_buffer.front() != '\0' ? error_buffer.data() : curl_easy_strerror(code));
    }
    lua_pushnumber(L, static_cast<double>(status));
    lua_pushlstring(L, response.data(), response.size());
    if (retry_after <= 0) {
        return 2;
    }
    lua_pushnumber(L, static_cast<double>(retry_after));
    return 3;
}

// Sets the request's body. libcurl does not copy it, so it must live until the request is done.
void set_body(CURL* curl, std::string_view body) {
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body.data());
    curl_easy_setopt(curl, CURLOPT_POSTFIELDSIZE_LARGE, static_cast<curl_off_t>(body.size()));
}

// Raises the error for a host.tool name that is not a tool, naming those that are.
[[noreturn]] void no_such_tool(lua_State* L, std::string_view name, const std::vector<Endpoint>& endpoints) {
    std::vector<std::string_view> names;
    for (const Endpoint& each : endpoints) {
        if (each.name != "llm") {
            names.push_back(each.name);
        }
    }
    std::string known;  // "a, b and c"
    for (std::size_t i = 0; i < names.size(); ++i) {
        if (i > 0) {
            known += i + 1 == names.size() ? " and " : ", ";
        }
        known += names[i];
    }
    luaL_error(L, "host.tool: no tool %s; the tools are %s", std::string(name).c_str(), known.c_str());
}

// The fields of the form table at idx, sorted by name, so a request is the same whatever order the table was built
// in.
std::vector<std::pair<std::string, std::string>> form_fields(lua_State* L, int idx) {
    std::vector<std::pair<std::string, std::string>> fields;
    lua_pushnil(L);
    while (lua_next(L, idx) != 0) {
        if (lua_type(L, -2) != LUA_TSTRING || lua_type(L, -1) != LUA_TSTRING) {
            luaL_error(L, "host.tool: a form is a table of strings by name");
        }
        std::size_t key_length = 0;
        std::size_t value_length = 0;
        const char* key = lua_tolstring(L, -2, &key_length);
        const char* value = lua_tolstring(L, -1, &value_length);
        fields.emplace_back(std::string(key, key_length), std::string(value, value_length));
        lua_pop(L, 1);
    }
    std::ranges::sort(fields);
    return fields;
}

// Attaches the file at path to part. Checked here, so a file that cannot be sent is reported as such rather than as
// a failed transfer: the error, or nothing when it is attached.
std::optional<std::string> attach_file(curl_mimepart* part, const std::string& path) {
    if (path.find('\0') != std::string::npos) {
        return "host.tool: the file name contains a NUL byte";
    }
    const UniqueFd file(::open(path.c_str(), O_RDONLY | O_CLOEXEC));
    struct stat st {};
    if (!file.valid() || fstat(file.get(), &st) != 0) {
        return fmt::format("{}: {}", path, error_text());
    }
    if (!S_ISREG(st.st_mode)) {
        return fmt::format("{}: not a regular file", path);
    }
    if (static_cast<std::size_t>(st.st_size) > kMaxUpload) {
        return fmt::format("{}: larger than 100 MiB", path);
    }
    if (curl_mime_filedata(part, path.c_str()) != CURLE_OK) {
        return fmt::format("{}: cannot be attached", path);
    }
    return std::nullopt;
}

// Fills mime with the form table at idx: `file` names a file to upload, and every other field is a text field.
// The error, or nothing when the form is complete.
std::optional<std::string> fill_form(lua_State* L, curl_mime* mime, int idx) {
    for (const auto& [key, value] : form_fields(L, idx)) {
        curl_mimepart* part = curl_mime_addpart(mime);
        curl_mime_name(part, key.c_str());
        if (key != "file") {
            curl_mime_data(part, value.data(), value.size());
        } else if (std::optional<std::string> error = attach_file(part, value)) {
            return error;
        }
    }
    return std::nullopt;
}

}  // namespace

const Endpoint* Host::endpoint(std::string_view name) const {
    const auto found = std::ranges::find(config_.endpoints, name, &Endpoint::name);
    return found == config_.endpoints.end() ? nullptr : &*found;
}

void Host::CurlDeleter::operator()(CURL* handle) const noexcept {
    curl_easy_cleanup(handle);
}

CURL* Host::curl() {
    if (!curl_) {
        curl_.reset(curl_easy_init());
    } else {
        curl_easy_reset(curl_.get());  // post resets it too, but a request that failed before reaching post may not
    }
    return curl_.get();
}

int Host::llm(lua_State* L) {
    std::size_t length = 0;
    const char* body = luaL_checklstring(L, 1, &length);
    Host& host = of(L);
    const double timeout = host.within_limit(option(L, 2, "timeout", 600));
    raise_if_signaled(L);  // a stop in progress starts no request
    const std::string beta = beta_option(L, 2);
    const Endpoint* endpoint = host.endpoint("llm");
    CURL* curl = host.curl();
    if (endpoint == nullptr || curl == nullptr) {
        return fail(L, "cannot start an HTTP request");
    }
    set_body(curl, {body, length});
    return post(L, curl, *endpoint,
                {"Content-Type: application/json", beta.empty() ? "" : fmt::format("anthropic-beta: {}", beta)},
                timeout, host.config_.ca_bundle);
}

// host.tool(name, request, {timeout=300}): a request to the endpoint of that name, other than host.llm's. Only the
// endpoints' names are taken, so each key reaches its own URL and nothing else. A string request is sent as JSON;
// for an endpoint that takes a form, a table is sent as a multipart form, in which `file` names a file to upload
// and every other field is a text field.
int Host::tool(lua_State* L) {
    const std::string_view name = check_cstring(L, 1);
    Host& host = of(L);
    const Endpoint* endpoint = name == "llm" ? nullptr : host.endpoint(name);
    if (endpoint == nullptr) {
        no_such_tool(L, name, host.config_.endpoints);
    }
    const bool form = endpoint->form && lua_istable(L, 2);
    if (!form) {
        luaL_checktype(L, 2, LUA_TSTRING);
    }
    const double timeout = host.within_limit(option(L, 3, "timeout", 300));
    raise_if_signaled(L);
    if (endpoint->key.empty() && !endpoint->key_env.empty()) {
        return fail(L, fmt::format("no {}: hx was started without one, so host.tool(\"{}\") cannot be used",
                                   endpoint->key_env, endpoint->name));
    }

    CURL* curl = host.curl();
    if (curl == nullptr) {
        return fail(L, "cannot start an HTTP request");
    }
    if (!form) {
        std::size_t length = 0;
        const char* body = lua_tolstring(L, 2, &length);
        set_body(curl, {body, length});
        return post(L, curl, *endpoint, {"Content-Type: application/json"}, timeout, host.config_.ca_bundle);
    }
    const CurlMime mime(curl_mime_init(curl));
    if (!mime) {
        return fail(L, "cannot start an HTTP request");
    }
    if (const std::optional<std::string> error = fill_form(L, mime.get(), 2)) {
        return fail(L, *error);
    }
    curl_easy_setopt(curl, CURLOPT_MIMEPOST, mime.get());
    return post(L, curl, *endpoint, {}, timeout, host.config_.ca_bundle);
}

}  // namespace hx
