#include "json.h"

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <memory>
#include <string>
#include <string_view>
#include <vector>

#include <fmt/format.h>
#include <yyjson.h>

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

struct MutDocDeleter {
    void operator()(yyjson_mut_doc* doc) const { yyjson_mut_doc_free(doc); }
};

struct TextDeleter {
    // NOLINTNEXTLINE(cppcoreguidelines-no-malloc)
    void operator()(char* text) const { std::free(text); }
};

using Doc = std::unique_ptr<yyjson_doc, DocDeleter>;
using MutDoc = std::unique_ptr<yyjson_mut_doc, MutDocDeleter>;
using Text = std::unique_ptr<char, TextDeleter>;

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
    const auto attach = [&]() {
        if (frames.empty()) {
            return;
        }
        Frame& frame = frames.back();
        if (frame.index != 0) {
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
            frames.push_back({unsafe_yyjson_get_first(value), count, array ? 1 : 0});
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
        if (frame.index == 0) {
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

std::string_view valid_utf8(std::string_view s, std::string& scratch) {
    std::size_t i = 0;
    while (i < s.size()) {
        const std::size_t length = utf8_length(s.substr(i));
        if (length == 0) {
            break;
        }
        i += length;
    }
    if (i == s.size()) {
        return s;
    }

    scratch.assign(s.substr(0, i));
    while (i < s.size()) {
        const std::size_t length = utf8_length(s.substr(i));
        if (length == 0) {
            scratch.append(kReplacement);
            ++i;
        } else {
            scratch.append(s.substr(i, length));
            i += length;
        }
    }
    return scratch;
}

class Encoder {
  public:
    Encoder(lua_State* L, yyjson_mut_doc* doc) : L_(L), doc_(doc) {}

    yyjson_mut_val* value(int idx, std::size_t depth) {
        switch (lua_type(L_, idx)) {
            case LUA_TNIL:
                return check(yyjson_mut_null(doc_));
            case LUA_TBOOLEAN:
                return check(yyjson_mut_bool(doc_, lua_toboolean(L_, idx) != 0));
            case LUA_TNUMBER:
                return number(lua_tonumber(L_, idx));
            case LUA_TSTRING: {
                std::size_t length = 0;
                const char* text = lua_tolstring(L_, idx, &length);
                return string({text, length});
            }
            case LUA_TTABLE:
                return table(idx, depth);
            case LUA_TLIGHTUSERDATA:
                if (is_null(L_, idx)) {
                    return check(yyjson_mut_null(doc_));
                }
                [[fallthrough]];
            default:
                luaL_error(L_, "json.encode: cannot encode a %s", luaL_typename(L_, idx));
        }
    }

  private:
    yyjson_mut_val* check(yyjson_mut_val* value) {
        if (value == nullptr) {
            luaL_error(L_, "json.encode: out of memory");
        }
        return value;
    }

    yyjson_mut_val* number(double n) {
        if (!std::isfinite(n)) {
            luaL_error(L_, "json.encode: cannot encode nan or inf");
        }
        if (std::floor(n) == n && std::fabs(n) < kExactIntegerLimit) {
            return check(yyjson_mut_sint(doc_, static_cast<std::int64_t>(n)));
        }
        return check(yyjson_mut_real(doc_, n));
    }

    yyjson_mut_val* string(std::string_view s) {
        const std::string_view clean = valid_utf8(s, scratch_);
        const bool copied = clean.data() == scratch_.data();
        return check(copied ? yyjson_mut_strncpy(doc_, clean.data(), clean.size())
                            : yyjson_mut_strn(doc_, clean.data(), clean.size()));
    }

    yyjson_mut_val* table(int idx, std::size_t depth) {
        if (depth >= kMaxDepth) {
            luaL_error(L_, "json.encode: nesting too deep (a table that contains itself?)");
        }
        luaL_checkstack(L_, 4, "json nesting");

        const auto length = static_cast<std::size_t>(lua_objlen(L_, idx));
        std::size_t count = 0;
        std::size_t indices = 0;
        std::vector<std::string_view> keys;
        lua_pushnil(L_);
        while (lua_next(L_, idx) != 0) {
            ++count;
            if (lua_type(L_, -2) == LUA_TSTRING) {
                std::size_t key_length = 0;
                const char* key = lua_tolstring(L_, -2, &key_length);
                keys.emplace_back(key, key_length);
            } else if (lua_type(L_, -2) == LUA_TNUMBER) {
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
            yyjson_mut_val* result = check(yyjson_mut_arr(doc_));
            for (std::size_t i = 1; i <= length; ++i) {
                lua_rawgeti(L_, idx, static_cast<int>(i));
                yyjson_mut_val* element = value(lua_gettop(L_), depth + 1);
                lua_pop(L_, 1);
                yyjson_mut_arr_append(result, element);
            }
            return result;
        }

        if (keys.size() != count) {
            luaL_error(L_, "json.encode: a table is neither an array (keys 1..n) nor an object (string keys)");
        }
        std::sort(keys.begin(), keys.end());
        yyjson_mut_val* result = check(yyjson_mut_obj(doc_));
        for (const std::string_view key : keys) {
            yyjson_mut_val* name = string(key);
            lua_pushlstring(L_, key.data(), key.size());
            lua_rawget(L_, idx);
            yyjson_mut_val* member = value(lua_gettop(L_), depth + 1);
            lua_pop(L_, 1);
            yyjson_mut_obj_add(result, name, member);
        }
        return result;
    }

    lua_State* L_;
    yyjson_mut_doc* doc_;
    std::string scratch_;
};

int l_encode(lua_State* L) {
    luaL_checkany(L, 1);
    lua_settop(L, 1);
    const MutDoc doc(yyjson_mut_doc_new(nullptr));
    if (doc == nullptr) {
        luaL_error(L, "json.encode: out of memory");
    }
    Encoder encoder(L, doc.get());
    yyjson_mut_doc_set_root(doc.get(), encoder.value(1, 0));
    std::size_t length = 0;
    yyjson_write_err error{};
    const Text text(yyjson_mut_write_opts(doc.get(), YYJSON_WRITE_NOFLAG, nullptr, &length, &error));
    if (text == nullptr) {
        luaL_error(L, "json.encode: %s", error.msg);
    }
    lua_pushlstring(L, text.get(), length);
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
