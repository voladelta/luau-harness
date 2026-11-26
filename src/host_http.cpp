#include <curl/curl.h>
#include <fcntl.h>
#include <sys/stat.h>

#include <algorithm>
#include <array>
#include <cctype>
#include <cmath>
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

constexpr std::size_t kMaxResponse = std::size_t{256} << 20;
constexpr std::size_t kMaxUpload = std::size_t{100} << 20;

struct ResponseBuffer {
    std::string body;
    bool too_large = false;
};

// NOLINTNEXTLINE(misc-const-correctness)
std::size_t on_body(char* data, std::size_t size, std::size_t count, void* user) noexcept {
    auto* out = static_cast<ResponseBuffer*>(user);
    const std::size_t room = kMaxResponse - out->body.size();
    if (size != 0 && count > room / size) {
        out->too_large = true;
        return 0;
    }
    const std::size_t bytes = size * count;
    if (bytes > room) {
        out->too_large = true;
        return 0;
    }
    try {
        out->body.append(data, bytes);
    } catch (...) {
        return 0;
    }
    return bytes;
}

int on_progress(void* /*user*/, curl_off_t /*dltotal*/, curl_off_t /*dlnow*/, curl_off_t /*ultotal*/,
                curl_off_t /*ulnow*/) noexcept {
    return pending_signal() != 0 ? 1 : 0;
}

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

constexpr std::string_view key_header(Auth auth) noexcept {
    switch (auth) {
        case Auth::Bearer:
            return "Authorization: Bearer ";
        case Auth::GoogleKey:
            return "x-goog-api-key: ";
        case Auth::ApiKey:
            break;
    }
    return "x-api-key: ";
}

int send(lua_State* L, CURL* curl, const Endpoint& endpoint, std::initializer_list<std::string_view> extra,
         double timeout, double deadline, const std::string& ca_bundle) {
    Headers headers;
    if (!endpoint.key.empty()) {
        headers.add(fmt::format("{}{}", key_header(endpoint.auth), endpoint.key));
    }
    for (const std::string_view header : extra) {
        if (!header.empty()) {
            headers.add(std::string(header));
        }
    }
    ResponseBuffer response;
    std::array<char, CURL_ERROR_SIZE> error_buffer{};
    curl_easy_setopt(curl, CURLOPT_URL, endpoint.url.c_str());
#if LIBCURL_VERSION_NUM >= 0x075500
    curl_easy_setopt(curl, CURLOPT_PROTOCOLS_STR, "http,https");
#else
    curl_easy_setopt(curl, CURLOPT_PROTOCOLS, CURLPROTO_HTTP | CURLPROTO_HTTPS);
#endif
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers.get());
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPALIVE, 1L);
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPIDLE, 60L);
    curl_easy_setopt(curl, CURLOPT_TCP_KEEPINTVL, 30L);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, on_body);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA, &response);
    curl_easy_setopt(curl, CURLOPT_MAXFILESIZE_LARGE, static_cast<curl_off_t>(kMaxResponse));
    curl_easy_setopt(curl, CURLOPT_XFERINFOFUNCTION, on_progress);
    curl_easy_setopt(curl, CURLOPT_NOPROGRESS, 0L);
    curl_easy_setopt(curl, CURLOPT_NOSIGNAL, 1L);
    curl_easy_setopt(curl, CURLOPT_CONNECTTIMEOUT, 30L);
    curl_easy_setopt(curl, CURLOPT_TIMEOUT_MS, std::max(1L, static_cast<long>(std::ceil(timeout * 1000.0))));
    curl_easy_setopt(curl, CURLOPT_ERRORBUFFER, error_buffer.data());
    if (!ca_bundle.empty()) {
        curl_easy_setopt(curl, CURLOPT_CAINFO, ca_bundle.c_str());
    }

    const CURLcode code = curl_easy_perform(curl);
    long status = 0;
    curl_off_t retry_after = 0;
    curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &status);
    curl_easy_getinfo(curl, CURLINFO_RETRY_AFTER, &retry_after);
    curl_easy_reset(curl);
    if (code != CURLE_OK) {
        raise_if_signaled(L);
        if (deadline > 0 && now() >= deadline) {
            keep_limit_check();
            raise_error(L, "timeout");
        }
        if (code == CURLE_FILESIZE_EXCEEDED || (code == CURLE_WRITE_ERROR && response.too_large)) {
            return fail(L, "the response body exceeds 256 MiB");
        }
        return fail(L, error_buffer.front() != '\0' ? error_buffer.data() : curl_easy_strerror(code));
    }
    lua_pushnumber(L, static_cast<double>(status));
    lua_pushlstring(L, response.body.data(), response.body.size());
    if (retry_after <= 0) {
        return 2;
    }
    lua_pushnumber(L, static_cast<double>(retry_after));
    return 3;
}

void set_body(CURL* curl, std::string_view body) {
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body.data());  // NOLINT(bugprone-suspicious-stringview-data-usage)
    curl_easy_setopt(curl, CURLOPT_POSTFIELDSIZE_LARGE, static_cast<curl_off_t>(body.size()));
}

[[noreturn]] void no_such_endpoint(lua_State* L, std::string_view name, const std::vector<Endpoint>& endpoints) {
    std::string known;
    for (std::size_t i = 0; i < endpoints.size(); ++i) {
        if (i > 0) {
            known += i + 1 == endpoints.size() ? " and " : ", ";
        }
        known += endpoints[i].name;
    }
    luaL_error(L, "host.post: no endpoint %s; the endpoints are %s", std::string(name).c_str(), known.c_str());
}

std::vector<std::pair<std::string, std::string>> form_fields(lua_State* L, int idx) {
    std::vector<std::pair<std::string, std::string>> fields;
    lua_pushnil(L);
    while (lua_next(L, idx) != 0) {
        if (lua_type(L, -2) != LUA_TSTRING || lua_type(L, -1) != LUA_TSTRING) {
            luaL_error(L, "host.post: a form is a table of strings by name");
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

std::optional<std::string> attach_file(curl_mimepart* part, const std::string& path) {
    if (path.find('\0') != std::string::npos) {
        return "the file name contains a NUL byte";
    }
    const UniqueFd file(::open(path.c_str(), O_RDONLY | O_CLOEXEC));
    struct stat st{};
    if (!file.valid() || fstat(file.get(), &st) != 0) {
        return fmt::format("{}: {}", path, error_text());
    }
    if (!S_ISREG(st.st_mode)) {
        return fmt::format("{}: not a regular file", path);
    }
    if (std::cmp_greater(st.st_size, kMaxUpload)) {
        return fmt::format("{}: larger than 100 MiB", path);
    }
    if (curl_mime_filedata(part, path.c_str()) != CURLE_OK) {
        return fmt::format("{}: cannot be attached", path);
    }
    return std::nullopt;
}

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

HttpClient::HttpClient(std::vector<Endpoint> endpoints, std::string ca_bundle)
    : endpoints_(std::move(endpoints)), ca_bundle_(std::move(ca_bundle)) {}

HttpClient::~HttpClient() = default;

const Endpoint* HttpClient::endpoint(std::string_view name) const {
    const auto found = std::ranges::find(endpoints_, name, &Endpoint::name);
    return found == endpoints_.end() ? nullptr : &*found;
}

void HttpClient::CurlDeleter::operator()(CURL* handle) const noexcept {
    curl_easy_cleanup(handle);
}

CURL* HttpClient::curl() {
    if (!curl_) {
        curl_.reset(curl_easy_init());
    } else {
        curl_easy_reset(curl_.get());
    }
    return curl_.get();
}

int HttpClient::post(lua_State* L, double deadline) {
    const std::string_view name = check_cstring(L, 1);
    const Endpoint* target = endpoint(name);
    if (target == nullptr) {
        no_such_endpoint(L, name, endpoints_);
    }
    const bool form = lua_istable(L, 2);
    if (!form) {
        luaL_checktype(L, 2, LUA_TSTRING);
    }
    const double timeout = within_limit(deadline, option(L, 3, "timeout", 600));
    raise_if_signaled(L);
    if (target->key.empty() && !target->key_env.empty()) {
        return fail(L, fmt::format("no {}: hx was started without one, so host.post cannot reach the {} endpoint",
                                   target->key_env, target->name));
    }
    CURL* handle = curl();
    if (handle == nullptr) {
        return fail(L, "cannot start an HTTP request");
    }
    if (!form) {
        std::size_t length = 0;
        const char* body = lua_tolstring(L, 2, &length);
        set_body(handle, {body, length});
        return send(L, handle, *target, {"Content-Type: application/json"}, timeout, deadline, ca_bundle_);
    }
    const CurlMime mime(curl_mime_init(handle));
    if (!mime) {
        return fail(L, "cannot start an HTTP request");
    }
    if (const std::optional<std::string> error = fill_form(L, mime.get(), 2)) {
        luaL_error(L, "host.post: %s", error->c_str());
    }
    curl_easy_setopt(handle, CURLOPT_MIMEPOST, mime.get());
    return send(L, handle, *target, {}, timeout, deadline, ca_bundle_);
}

}  // namespace hx
