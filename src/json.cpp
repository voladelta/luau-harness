#include "json.h"

#include <algorithm>
#include <charconv>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <string>
#include <string_view>
#include <vector>

#include "lua.h"
#include "lualib.h"

namespace hx {
namespace {

constexpr int kMaxDepth = 200;
constexpr char kArrayMeta[] = "hx.json.array";

// Only its address is used: the light userdata behind json.null.
char null_tag;

bool is_null(lua_State* L, int idx) {
    return lua_islightuserdata(L, idx) && lua_touserdata(L, idx) == &null_tag;
}

void mark_array(lua_State* L) {
    luaL_getmetatable(L, kArrayMeta);
    lua_setmetatable(L, -2);
}

bool is_marked_array(lua_State* L, int idx) {
    if (!lua_getmetatable(L, idx)) return false;
    luaL_getmetatable(L, kArrayMeta);
    const bool same = lua_rawequal(L, -1, -2) != 0;
    lua_pop(L, 2);
    return same;
}

void append_utf8(std::string& out, uint32_t cp) {
    if (cp < 0x80) {
        out.push_back(static_cast<char>(cp));
    } else if (cp < 0x800) {
        out.push_back(static_cast<char>(0xC0 | (cp >> 6)));
        out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
    } else if (cp < 0x10000) {
        out.push_back(static_cast<char>(0xE0 | (cp >> 12)));
        out.push_back(static_cast<char>(0x80 | ((cp >> 6) & 0x3F)));
        out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
    } else {
        out.push_back(static_cast<char>(0xF0 | (cp >> 18)));
        out.push_back(static_cast<char>(0x80 | ((cp >> 12) & 0x3F)));
        out.push_back(static_cast<char>(0x80 | ((cp >> 6) & 0x3F)));
        out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
    }
}

// Recursive-descent parser that builds the value directly on the Lua stack.
class Parser {
  public:
    Parser(lua_State* L, std::string_view text)
        : L_(L), begin_(text.data()), p_(text.data()), end_(text.data() + text.size()) {}

    // Pushes the decoded value and returns true. On false, error() says why and the caller resets the stack.
    bool document() {
        if (!value(0)) return false;
        skip_space();
        if (p_ != end_) return fail("unexpected trailing text");
        return true;
    }

    const std::string& error() const { return error_; }

  private:
    bool fail(const char* what) {
        if (error_.empty()) error_ = std::string(what) + " at byte " + std::to_string(p_ - begin_ + 1);
        return false;
    }

    void skip_space() {
        while (p_ != end_ && (*p_ == ' ' || *p_ == '\n' || *p_ == '\t' || *p_ == '\r')) ++p_;
    }

    bool literal(std::string_view word) {
        if (static_cast<size_t>(end_ - p_) < word.size() || std::memcmp(p_, word.data(), word.size()) != 0)
            return fail("invalid value");
        p_ += word.size();
        return true;
    }

    bool value(int depth) {
        if (depth > kMaxDepth) return fail("nesting too deep");
        luaL_checkstack(L_, 4, "json nesting");
        skip_space();
        if (p_ == end_) return fail("unexpected end of text");
        switch (*p_) {
        case '{':
            return object(depth);
        case '[':
            return array(depth);
        case '"':
            return string();
        case 't':
            if (!literal("true")) return false;
            lua_pushboolean(L_, 1);
            return true;
        case 'f':
            if (!literal("false")) return false;
            lua_pushboolean(L_, 0);
            return true;
        case 'n':
            if (!literal("null")) return false;
            lua_pushlightuserdata(L_, &null_tag);
            return true;
        default:
            return number();
        }
    }

    bool number() {
        if (*p_ != '-' && (*p_ < '0' || *p_ > '9')) return fail("invalid value");
        double n = 0;
        const auto [next, ec] = std::from_chars(p_, end_, n);
        if (ec != std::errc() || !std::isfinite(n)) return fail("invalid number");
        p_ = next;
        lua_pushnumber(L_, n);
        return true;
    }

    bool hex4(uint32_t& out) {
        if (end_ - p_ < 4) return fail("truncated \\u escape");
        out = 0;
        for (int i = 0; i < 4; ++i) {
            const char c = *p_++;
            uint32_t digit;
            if (c >= '0' && c <= '9') digit = static_cast<uint32_t>(c - '0');
            else if (c >= 'a' && c <= 'f') digit = static_cast<uint32_t>(c - 'a' + 10);
            else if (c >= 'A' && c <= 'F') digit = static_cast<uint32_t>(c - 'A' + 10);
            else return fail("invalid \\u escape");
            out = out * 16 + digit;
        }
        return true;
    }

    // Reads the code point of a \u escape whose "\u" is already consumed, joining a surrogate pair. An unpaired
    // surrogate becomes U+FFFD.
    bool unicode_escape(uint32_t& cp) {
        if (!hex4(cp)) return false;
        if (cp >= 0xDC00 && cp <= 0xDFFF) {
            cp = 0xFFFD;
        } else if (cp >= 0xD800 && cp <= 0xDBFF) {
            const char* const after_high = p_;
            uint32_t low = 0;
            if (end_ - p_ >= 6 && p_[0] == '\\' && p_[1] == 'u') {
                p_ += 2;
                if (!hex4(low)) return false;
            }
            if (low >= 0xDC00 && low <= 0xDFFF) {
                cp = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
            } else {
                p_ = after_high;
                cp = 0xFFFD;
            }
        }
        return true;
    }

    // Raw control characters inside a string are accepted: models emit them in tool arguments.
    bool string() {
        ++p_;
        const char* const start = p_;
        while (p_ != end_ && *p_ != '"' && *p_ != '\\') ++p_;
        if (p_ == end_) return fail("unterminated string");
        if (*p_ == '"') {
            lua_pushlstring(L_, start, static_cast<size_t>(p_ - start));
            ++p_;
            return true;
        }
        std::string out(start, p_);
        for (;;) {
            if (p_ == end_) return fail("unterminated string");
            const char c = *p_++;
            if (c == '"') break;
            if (c != '\\') {
                out.push_back(c);
                continue;
            }
            if (p_ == end_) return fail("unterminated string");
            switch (*p_++) {
            case '"': out.push_back('"'); break;
            case '\\': out.push_back('\\'); break;
            case '/': out.push_back('/'); break;
            case 'b': out.push_back('\b'); break;
            case 'f': out.push_back('\f'); break;
            case 'n': out.push_back('\n'); break;
            case 'r': out.push_back('\r'); break;
            case 't': out.push_back('\t'); break;
            case 'u': {
                uint32_t cp = 0;
                if (!unicode_escape(cp)) return false;
                append_utf8(out, cp);
                break;
            }
            default:
                return fail("invalid escape");
            }
        }
        lua_pushlstring(L_, out.data(), out.size());
        return true;
    }

    bool array(int depth) {
        ++p_;
        lua_newtable(L_);
        mark_array(L_);
        skip_space();
        if (p_ != end_ && *p_ == ']') {
            ++p_;
            return true;
        }
        for (int i = 1;; ++i) {
            if (!value(depth + 1)) return false;
            lua_rawseti(L_, -2, i);
            skip_space();
            if (p_ == end_) return fail("unterminated array");
            const char c = *p_++;
            if (c == ']') return true;
            if (c != ',') {
                --p_;
                return fail("expected ',' or ']'");
            }
        }
    }

    bool object(int depth) {
        ++p_;
        lua_newtable(L_);
        skip_space();
        if (p_ != end_ && *p_ == '}') {
            ++p_;
            return true;
        }
        for (;;) {
            skip_space();
            if (p_ == end_ || *p_ != '"') return fail("expected a string key");
            if (!string()) return false;
            skip_space();
            if (p_ == end_ || *p_ != ':') return fail("expected ':'");
            ++p_;
            if (!value(depth + 1)) return false;
            if (is_null(L_, -1)) lua_pop(L_, 2);
            else lua_rawset(L_, -3);
            skip_space();
            if (p_ == end_) return fail("unterminated object");
            const char c = *p_++;
            if (c == '}') return true;
            if (c != ',') {
                --p_;
                return fail("expected ',' or '}'");
            }
        }
    }

    lua_State* L_;
    const char* begin_;
    const char* p_;
    const char* end_;
    std::string error_;
};

// Length of the well-formed UTF-8 sequence at s (1 to 4), or 0 when the bytes there are not one.
size_t utf8_length(const unsigned char* s, size_t left) {
    const unsigned char lead = s[0];
    if (lead < 0x80) return 1;
    size_t len;
    unsigned char low = 0x80;
    unsigned char high = 0xBF;
    if (lead >= 0xC2 && lead <= 0xDF) {
        len = 2;
    } else if (lead >= 0xE0 && lead <= 0xEF) {
        len = 3;
        if (lead == 0xE0) low = 0xA0;  // no overlong forms
        if (lead == 0xED) high = 0x9F; // no surrogates
    } else if (lead >= 0xF0 && lead <= 0xF4) {
        len = 4;
        if (lead == 0xF0) low = 0x90;
        if (lead == 0xF4) high = 0x8F; // nothing above U+10FFFF
    } else {
        return 0;
    }
    if (left < len || s[1] < low || s[1] > high) return 0;
    for (size_t i = 2; i < len; ++i)
        if (s[i] < 0x80 || s[i] > 0xBF) return 0;
    return len;
}

class Encoder {
  public:
    explicit Encoder(lua_State* L) : L_(L) {}

    // Appends the value at absolute stack index idx to out. On false, error says why.
    bool value(int idx, int depth) {
        switch (lua_type(L_, idx)) {
        case LUA_TNIL:
            out.append("null");
            return true;
        case LUA_TBOOLEAN:
            out.append(lua_toboolean(L_, idx) ? "true" : "false");
            return true;
        case LUA_TNUMBER:
            return number(lua_tonumber(L_, idx));
        case LUA_TSTRING: {
            size_t len = 0;
            const char* s = lua_tolstring(L_, idx, &len);
            string({s, len});
            return true;
        }
        case LUA_TTABLE:
            return table(idx, depth);
        case LUA_TLIGHTUSERDATA:
            if (is_null(L_, idx)) {
                out.append("null");
                return true;
            }
            [[fallthrough]];
        default:
            return fail(std::string("cannot encode a ") + luaL_typename(L_, idx));
        }
    }

    std::string out;
    std::string error;

  private:
    bool fail(std::string what) {
        if (error.empty()) error = std::move(what);
        return false;
    }

    bool number(double n) {
        if (!std::isfinite(n)) return fail("cannot encode nan or inf");
        char buf[32];
        std::to_chars_result r{};
        if (std::floor(n) == n && std::fabs(n) < 9007199254740992.0) // exactly an integer: no exponent, no ".0"
            r = std::to_chars(buf, buf + sizeof buf, static_cast<long long>(n));
        else
            r = std::to_chars(buf, buf + sizeof buf, n);
        out.append(buf, r.ptr);
        return true;
    }

    void string(std::string_view s) {
        static constexpr char kHex[] = "0123456789abcdef";
        const auto* bytes = reinterpret_cast<const unsigned char*>(s.data());
        out.push_back('"');
        for (size_t i = 0; i < s.size();) {
            const unsigned char c = bytes[i];
            if (c >= 0x80) {
                const size_t len = utf8_length(bytes + i, s.size() - i);
                if (len == 0) {
                    out.append("\xEF\xBF\xBD");
                    ++i;
                } else {
                    out.append(s.substr(i, len));
                    i += len;
                }
                continue;
            }
            ++i;
            switch (c) {
            case '"': out.append("\\\""); break;
            case '\\': out.append("\\\\"); break;
            case '\n': out.append("\\n"); break;
            case '\r': out.append("\\r"); break;
            case '\t': out.append("\\t"); break;
            default:
                if (c < 0x20) {
                    out.append("\\u00");
                    out.push_back(kHex[c >> 4]);
                    out.push_back(kHex[c & 0xF]);
                } else {
                    out.push_back(static_cast<char>(c));
                }
            }
        }
        out.push_back('"');
    }

    bool table(int idx, int depth) {
        if (depth >= kMaxDepth) return fail("nesting too deep (a table that contains itself?)");
        luaL_checkstack(L_, 4, "json nesting");

        const size_t length = static_cast<size_t>(lua_objlen(L_, idx));
        size_t count = 0;
        std::vector<std::string_view> keys; // views into strings the table keeps alive
        lua_pushnil(L_);
        while (lua_next(L_, idx) != 0) {
            ++count;
            if (lua_type(L_, -2) == LUA_TSTRING) {
                size_t len = 0;
                const char* s = lua_tolstring(L_, -2, &len);
                keys.emplace_back(s, len);
            }
            lua_pop(L_, 1);
        }

        const bool marked = is_marked_array(L_, idx);
        if (marked || (length > 0 && count == length)) {
            if (count != length) return fail("a json.array may only have the keys 1..n");
            out.push_back('[');
            for (size_t i = 1; i <= length; ++i) {
                if (i > 1) out.push_back(',');
                lua_rawgeti(L_, idx, static_cast<int>(i));
                const bool ok = value(lua_gettop(L_), depth + 1);
                lua_pop(L_, 1);
                if (!ok) return false;
            }
            out.push_back(']');
            return true;
        }

        if (keys.size() != count) return fail("a table is neither an array (keys 1..n) nor an object (string keys)");
        std::sort(keys.begin(), keys.end());
        out.push_back('{');
        for (size_t i = 0; i < keys.size(); ++i) {
            if (i > 0) out.push_back(',');
            string(keys[i]);
            out.push_back(':');
            lua_pushlstring(L_, keys[i].data(), keys[i].size());
            lua_rawget(L_, idx);
            const bool ok = value(lua_gettop(L_), depth + 1);
            lua_pop(L_, 1);
            if (!ok) return false;
        }
        out.push_back('}');
        return true;
    }

    lua_State* L_;
};

int l_encode(lua_State* L) {
    luaL_checkany(L, 1);
    lua_settop(L, 1);
    Encoder encoder(L);
    if (!encoder.value(1, 0)) luaL_error(L, "json.encode: %s", encoder.error.c_str());
    lua_pushlstring(L, encoder.out.data(), encoder.out.size());
    return 1;
}

int l_decode(lua_State* L) {
    size_t len = 0;
    const char* text = luaL_checklstring(L, 1, &len);
    const int top = lua_gettop(L);
    Parser parser(L, {text, len});
    if (parser.document()) return 1;
    lua_settop(L, top);
    lua_pushnil(L);
    lua_pushlstring(L, parser.error().data(), parser.error().size());
    return 2;
}

int l_array(lua_State* L) {
    if (lua_isnoneornil(L, 1)) {
        lua_settop(L, 0);
        lua_newtable(L);
    } else {
        luaL_checktype(L, 1, LUA_TTABLE);
        lua_settop(L, 1);
    }
    mark_array(L);
    return 1;
}

} // namespace

void open_json(lua_State* L) {
    luaL_newmetatable(L, kArrayMeta);
    lua_pop(L, 1);

    static const luaL_Reg functions[] = {
        {"encode", l_encode},
        {"decode", l_decode},
        {"array", l_array},
        {nullptr, nullptr},
    };
    luaL_register(L, "json", functions);
    lua_pushlightuserdata(L, &null_tag);
    lua_setfield(L, -2, "null");
    lua_pop(L, 1);
}

} // namespace hx
