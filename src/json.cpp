#include "json.h"

#include <algorithm>
#include <array>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>
#include <string_view>
#include <vector>

#include <fmt/format.h>
#include <yyjson.h>

#include "host_internal.h"
#include "lua.h"
#include "lualib.h"

namespace hx {
namespace {

constexpr std::size_t kMaxDepth = YYJSON_READER_DEPTH_LIMIT;
static_assert(kMaxDepth > 0);
constexpr char kArrayMeta[] = "hx.json.array";
constexpr std::string_view kReplacement = "\xEF\xBF\xBD";
constexpr double kExactIntegerLimit = 9007199254740992.0;

// NOLINTNEXTLINE(cppcoreguidelines-avoid-non-const-global-variables)
constinit char null_tag = 0;

struct DocDeleter {
    void operator()(yyjson_doc* doc) const { yyjson_doc_free(doc); }
};

using Doc = std::unique_ptr<yyjson_doc, DocDeleter>;

bool is_null(lua_State* L, int idx) {
    return lua_islightuserdata(L, idx) && lua_touserdata(L, idx) == &null_tag;
}

void mark_array(lua_State* L) {
    luaL_getmetatable(L, kArrayMeta);
    lua_setmetatable(L, -2);
}

bool is_marked_array(lua_State* L, int idx) {
    if (lua_getmetatable(L, idx) == 0) {
        return false;
    }
    luaL_getmetatable(L, kArrayMeta);
    const bool same = lua_rawequal(L, -1, -2) != 0;
    lua_pop(L, 2);
    return same;
}

void push_scalar(lua_State* L, const yyjson_val* value) {
    switch (yyjson_get_type(value)) {
        case YYJSON_TYPE_NULL:
            lua_pushlightuserdata(L, &null_tag);
            break;
        case YYJSON_TYPE_BOOL:
            lua_pushboolean(L, yyjson_get_bool(value) ? 1 : 0);
            break;
        case YYJSON_TYPE_STR:
            lua_pushlstring(L, yyjson_get_str(value), yyjson_get_len(value));
            break;
        default:
            lua_pushnumber(L, yyjson_get_num(value));
            break;
    }
}

struct Frame {
    yyjson_val* next;
    std::size_t left;
    bool array;
    int index;
};

int l_decode(lua_State* L) {
    std::size_t length = 0;
    const char* text = luaL_checklstring(L, 1, &length);
    yyjson_read_err error{};
    const Doc doc(yyjson_read_opts(
        // NOLINTNEXTLINE(cppcoreguidelines-pro-type-const-cast)
        const_cast<char*>(text), length, YYJSON_READ_ALLOW_BOM | YYJSON_READ_ALLOW_INVALID_UNICODE, nullptr, &error));
    if (doc == nullptr) {
        const std::string message = fmt::format("{} (at byte {})", error.msg, error.pos + 1);
        lua_pushnil(L);
        lua_pushlstring(L, message.data(), message.size());
        return 2;
    }

    std::vector<Frame> frames;
    const auto attach = [&] {
        if (frames.empty()) {
            return;
        }
        Frame& frame = frames.back();
        if (frame.array) {
            lua_rawseti(L, -2, frame.index++);
        } else if (is_null(L, -1)) {
            lua_pop(L, 2);
        } else {
            lua_rawset(L, -3);
        }
    };

    const yyjson_val* value = yyjson_doc_get_root(doc.get());
    while (true) {
        if (yyjson_is_ctn(value)) {
            const bool array = yyjson_is_arr(value);
            const std::size_t count = array ? yyjson_arr_size(value) : yyjson_obj_size(value);
            const auto reserve = static_cast<int>(count);
            luaL_checkstack(L, 3, "json nesting");
            lua_createtable(L, array ? reserve : 0, array ? 0 : reserve);
            if (array) {
                mark_array(L);
            }
            frames.push_back({unsafe_yyjson_get_first(value), count, array, 1});
        } else {
            push_scalar(L, value);
            attach();
        }

        while (!frames.empty() && frames.back().left == 0) {
            frames.pop_back();
            attach();
        }
        if (frames.empty()) {
            return 1;
        }
        Frame& frame = frames.back();
        --frame.left;
        if (!frame.array) {
            lua_pushlstring(L, yyjson_get_str(frame.next), yyjson_get_len(frame.next));
            frame.next = unsafe_yyjson_get_next(frame.next);
        }
        value = frame.next;
        frame.next = unsafe_yyjson_get_next(value);
    }
}

std::size_t utf8_length(std::string_view s) {
    const auto byte = [&](std::size_t i) { return static_cast<unsigned char>(s[i]); };
    const unsigned char lead = byte(0);
    if (lead < 0x80) {
        return 1;
    }
    std::size_t length = 0;
    unsigned char low = 0x80;
    unsigned char high = 0xBF;
    if (lead >= 0xC2 && lead <= 0xDF) {
        length = 2;
    } else if (lead >= 0xE0 && lead <= 0xEF) {
        length = 3;
        if (lead == 0xE0) {
            low = 0xA0;
        }
        if (lead == 0xED) {
            high = 0x9F;
        }
    } else if (lead >= 0xF0 && lead <= 0xF4) {
        length = 4;
        if (lead == 0xF0) {
            low = 0x90;
        }
        if (lead == 0xF4) {
            high = 0x8F;
        }
    } else {
        return 0;
    }
    if (s.size() < length || byte(1) < low || byte(1) > high) {
        return 0;
    }
    for (std::size_t i = 2; i < length; ++i) {
        if (byte(i) < 0x80 || byte(i) > 0xBF) {
            return 0;
        }
    }
    return length;
}

constexpr std::array<bool, 256> kPlain = [] {
    std::array<bool, 256> plain{};
    for (std::size_t c = 0x20; c < 0x80; ++c) {
        plain.at(c) = c != '"' && c != '\\';
    }
    return plain;
}();

std::string_view escape(unsigned char c, std::array<char, 6>& buffer) {
    switch (c) {
        case '"':
            return "\\\"";
        case '\\':
            return "\\\\";
        case '\b':
            return "\\b";
        case '\f':
            return "\\f";
        case '\n':
            return "\\n";
        case '\r':
            return "\\r";
        case '\t':
            return "\\t";
        default: {
            constexpr std::string_view kHex = "0123456789ABCDEF";
            buffer = {'\\', 'u', '0', '0', kHex[c >> 4], kHex[c & 0xF]};
            return {buffer.data(), buffer.size()};
        }
    }
}

constexpr int kValues = 2;
constexpr int kMaxIndent = 16;

class Encoder {
  public:
    Encoder(lua_State* L, std::size_t indent) : L_(L), indent_(indent) {}

    void value(int idx, std::size_t depth) {
        switch (lua_type(L_, idx)) {
            case LUA_TNIL:
                out_ += "null";
                return;
            case LUA_TBOOLEAN:
                out_ += lua_toboolean(L_, idx) != 0 ? "true" : "false";
                return;
            case LUA_TNUMBER:
                number(lua_tonumber(L_, idx));
                return;
            case LUA_TSTRING: {
                std::size_t length = 0;
                const char* text = lua_tolstring(L_, idx, &length);
                string({text, length});
                return;
            }
            case LUA_TTABLE:
                table(idx, depth);
                return;
            case LUA_TLIGHTUSERDATA:
                if (is_null(L_, idx)) {
                    out_ += "null";
                    return;
                }
                [[fallthrough]];
            default:
                luaL_error(L_, "json.encode: cannot encode a %s", luaL_typename(L_, idx));
        }
    }

    [[nodiscard]] const std::string& text() const { return out_; }

  private:
    void number(double n) {
        if (!std::isfinite(n)) {
            luaL_error(L_, "json.encode: cannot encode nan or inf");
        }
        yyjson_val number{};
        if (std::floor(n) == n && std::fabs(n) < kExactIntegerLimit) {
            yyjson_set_sint(&number, static_cast<std::int64_t>(n));
        } else {
            yyjson_set_real(&number, n);
        }
        std::array<char, 40> digits{};
        char* end = yyjson_write_number(&number, digits.data());
        out_.append(digits.data(), end);
    }

    void string(std::string_view s) {
        out_ += '"';
        std::array<char, 6> buffer{};
        const char* run = s.data();
        const char* p = run;
        const char* const end = p + s.size();
        while (true) {
            while (p != end && kPlain.at(static_cast<unsigned char>(*p))) {
                ++p;
            }
            if (p == end) {
                break;
            }
            const auto c = static_cast<unsigned char>(*p);
            if (c >= 0x80) {
                const std::size_t length = utf8_length({p, static_cast<std::size_t>(end - p)});
                if (length > 0) {
                    p += length;
                    continue;
                }
                out_.append(run, p).append(kReplacement);
            } else {
                out_.append(run, p).append(escape(c, buffer));
            }
            run = ++p;
        }
        out_.append(run, end) += '"';
    }

    void table(int idx, std::size_t depth) {
        if (depth >= kMaxDepth) {
            luaL_error(L_, "json.encode: nesting too deep (a table that contains itself?)");
        }
        luaL_checkstack(L_, 4, "json nesting");

        const auto length = static_cast<std::size_t>(lua_objlen(L_, idx));
        const std::size_t base = members_.size();
        std::size_t count = 0;
        std::size_t indices = 0;
        lua_pushnil(L_);
        while (lua_next(L_, idx) != 0) {
            ++count;
            if (lua_type(L_, -2) == LUA_TSTRING) {
                std::size_t key_length = 0;
                const char* key = lua_tolstring(L_, -2, &key_length);
                const auto slot = static_cast<int>(members_.size()) + 1;
                members_.push_back({{key, key_length}, slot});
                lua_rawseti(L_, kValues, slot);
                continue;
            }
            if (lua_type(L_, -2) == LUA_TNUMBER) {
                const double key = lua_tonumber(L_, -2);
                if (key >= 1 && key <= static_cast<double>(length) && std::floor(key) == key) {
                    ++indices;
                }
            }
            lua_pop(L_, 1);
        }

        const bool array = indices == count && count == length;
        if (is_marked_array(L_, idx) || (length > 0 && array)) {
            if (!array) {
                luaL_error(L_, "json.encode: a json.array may only have the keys 1..n");
            }
            out_ += '[';
            for (std::size_t i = 1; i <= length; ++i) {
                if (i > 1) {
                    out_ += ',';
                }
                newline(depth + 1);
                lua_rawgeti(L_, idx, static_cast<int>(i));
                value(lua_gettop(L_), depth + 1);
                lua_pop(L_, 1);
            }
            if (length > 0) {
                newline(depth);
            }
            out_ += ']';
            return;
        }

        if (members_.size() - base != count) {
            luaL_error(L_, "json.encode: a table is neither an array (keys 1..n) nor an object (string keys)");
        }
        const auto first = members_.begin() + static_cast<std::ptrdiff_t>(base);
        std::sort(first, members_.end(), [](const Member& a, const Member& b) { return a.key < b.key; });
        out_ += '{';
        for (std::size_t i = base; i < members_.size(); ++i) {
            const Member member = members_[i];
            if (i > base) {
                out_ += ',';
            }
            newline(depth + 1);
            string(member.key);
            out_ += indent_ > 0 ? ": " : ":";
            lua_rawgeti(L_, kValues, member.slot);
            value(lua_gettop(L_), depth + 1);
            lua_pop(L_, 1);
        }
        if (count > 0) {
            newline(depth);
        }
        members_.resize(base);
        out_ += '}';
    }

    void newline(std::size_t depth) {
        if (indent_ > 0) {
            out_ += '\n';
            out_.append(depth * indent_, ' ');
        }
    }

    struct Member {
        std::string_view key;
        int slot = 0;
    };

    lua_State* L_;
    std::size_t indent_;
    std::string out_;
    std::vector<Member> members_;
};

int l_encode(lua_State* L) {
    luaL_checkany(L, 1);
    const double indent = option(L, 2, "indent", 0);
    if (indent < 0 || indent > kMaxIndent || std::floor(indent) != indent) {
        luaL_error(L, "json.encode: indent is a whole number of spaces from 0 to %d", kMaxIndent);
    }
    lua_settop(L, 1);
    lua_newtable(L);
    Encoder encoder(L, static_cast<std::size_t>(indent));
    encoder.value(1, 0);
    lua_pushlstring(L, encoder.text().data(), encoder.text().size());
    return 1;
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

}  // namespace

void open_json(lua_State* L) {
    luaL_newmetatable(L, kArrayMeta);
    lua_pop(L, 1);

    static constexpr luaL_Reg kFunctions[] = {
        {"encode", l_encode},
        {"decode", l_decode},
        {"array", l_array},
        {nullptr, nullptr},
    };
    luaL_register(L, "json", kFunctions);
    lua_pushlightuserdata(L, &null_tag);
    lua_setfield(L, -2, "null");
    lua_pop(L, 1);
}

}  // namespace hx
