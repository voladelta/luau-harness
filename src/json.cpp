#include "json.h"

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <limits>
#include <string>
#include <string_view>
#include <vector>

#include <fmt/format.h>
#include <rapidjson/encodedstream.h>
#include <rapidjson/error/en.h>
#include <rapidjson/memorystream.h>
#include <rapidjson/reader.h>
#include <rapidjson/stringbuffer.h>
#include <rapidjson/writer.h>

#include "lua.h"
#include "lualib.h"

namespace hx {
namespace {

constexpr std::size_t kMaxDepth = 200;
constexpr char kArrayMeta[] = "hx.json.array";
constexpr std::string_view kReplacement = "\xEF\xBF\xBD";  // U+FFFD
// Largest magnitude below which every integral double is an exact integer.
constexpr double kExactIntegerLimit = 9007199254740992.0;

// The light userdata behind json.null. Only its address is used; nothing reads or writes it.
// NOLINTNEXTLINE(cppcoreguidelines-avoid-non-const-global-variables): lua_pushlightuserdata takes a void*
constinit char null_tag = 0;

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

// --- decoding ---------------------------------------------------------------------------------------------

// RapidJSON SAX handler that builds the decoded value on the Lua stack while the reader walks the text, so no
// intermediate document is allocated. The method names are the ones the reader calls.
class LuaBuilder : public rapidjson::BaseReaderHandler<rapidjson::UTF8<>, LuaBuilder> {
  public:
    explicit LuaBuilder(lua_State* L) : L_(L) {}

    bool Null() {
        lua_pushlightuserdata(L_, &null_tag);
        return attach();
    }
    bool Bool(bool value) {
        lua_pushboolean(L_, value ? 1 : 0);
        return attach();
    }
    bool Int(int value) { return number(value); }
    bool Uint(unsigned value) { return number(value); }
    bool Int64(std::int64_t value) { return number(static_cast<double>(value)); }
    bool Uint64(std::uint64_t value) { return number(static_cast<double>(value)); }
    bool Double(double value) { return number(value); }
    bool String(const char* text, rapidjson::SizeType length, bool /*copy*/) {
        lua_pushlstring(L_, text, length);
        return attach();
    }
    bool Key(const char* text, rapidjson::SizeType length, bool /*copy*/) {
        lua_pushlstring(L_, text, length);
        return true;
    }
    bool StartObject() { return open(false); }
    bool EndObject(rapidjson::SizeType /*members*/) { return close(); }
    bool StartArray() { return open(true); }
    bool EndArray(rapidjson::SizeType /*elements*/) { return close(); }

    [[nodiscard]] bool too_deep() const { return too_deep_; }

  private:
    struct Frame {
        bool array;
        int next;  // index of the next element, for arrays
    };

    bool number(double value) {
        lua_pushnumber(L_, value);
        return attach();
    }

    bool open(bool array) {
        if (frames_.size() >= kMaxDepth) {
            too_deep_ = true;
            return false;  // stops the reader
        }
        luaL_checkstack(L_, 4, "json nesting");
        lua_newtable(L_);
        if (array) {
            mark_array(L_);
        }
        frames_.push_back({array, 1});
        return true;
    }

    bool close() {
        frames_.pop_back();
        return attach();
    }

    // Stores the value on top of the stack into the container being built. The root value stays where it is.
    bool attach() {
        if (frames_.empty()) {
            return true;
        }
        Frame& frame = frames_.back();
        if (frame.array) {
            lua_rawseti(L_, -2, frame.next++);
        } else if (is_null(L_, -1)) {
            lua_pop(L_, 2);  // a null member is an absent key
        } else {
            lua_rawset(L_, -3);
        }
        return true;
    }

    lua_State* L_;
    std::vector<Frame> frames_;
    bool too_deep_ = false;
};

int l_decode(lua_State* L) {
    std::size_t length = 0;
    const char* text = luaL_checklstring(L, 1, &length);
    const int top = lua_gettop(L);

    LuaBuilder builder(L);
    rapidjson::MemoryStream memory(text, length);
    rapidjson::EncodedInputStream<rapidjson::UTF8<>, rapidjson::MemoryStream> stream(memory);
    rapidjson::Reader reader;
    // Iterative: nesting depth cannot overflow the C stack. Full precision: numbers round-trip exactly.
    const rapidjson::ParseResult result =
        reader.Parse<rapidjson::kParseIterativeFlag | rapidjson::kParseFullPrecisionFlag>(stream, builder);
    // The reader takes a NUL byte for the end of the text, so make sure it really was the end.
    const bool stopped_early = !result.IsError() && memory.Tell() < length;
    if (!result.IsError() && !stopped_early) {
        return 1;
    }

    lua_settop(L, top);
    std::string message;
    if (stopped_early) {
        message =
            fmt::format("The document root must not be followed by other values. (at byte {})", memory.Tell() + 1);
    } else {
        message = fmt::format("{} (at byte {})",
                              builder.too_deep() ? "Nesting too deep." : rapidjson::GetParseError_En(result.Code()),
                              result.Offset() + 1);
    }
    lua_pushnil(L);
    lua_pushlstring(L, message.data(), message.size());
    return 2;
}

// --- encoding ---------------------------------------------------------------------------------------------

// Length of the well-formed UTF-8 sequence at the start of s (1 to 4), or 0 when the bytes there are not one.
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
            low = 0xA0;  // no overlong forms
        }
        if (lead == 0xED) {
            high = 0x9F;  // no surrogates
        }
    } else if (lead >= 0xF0 && lead <= 0xF4) {
        length = 4;
        if (lead == 0xF0) {
            low = 0x90;
        }
        if (lead == 0xF4) {
            high = 0x8F;  // nothing above U+10FFFF
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

// Returns s itself when it is valid UTF-8. Otherwise fills `scratch` with a copy in which every bad byte is
// U+FFFD, and returns a view of that.
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

// Walks a Lua value and feeds it to a RapidJSON writer.
class Encoder {
  public:
    explicit Encoder(lua_State* L) : L_(L), writer_(buffer_) {}

    // Writes the value at absolute stack index idx. On false, error() says why.
    bool value(int idx, std::size_t depth) {
        switch (lua_type(L_, idx)) {
            case LUA_TNIL:
                return writer_.Null();
            case LUA_TBOOLEAN:
                return writer_.Bool(lua_toboolean(L_, idx) != 0);
            case LUA_TNUMBER:
                return number(lua_tonumber(L_, idx));
            case LUA_TSTRING: {
                std::size_t length = 0;
                const char* text = lua_tolstring(L_, idx, &length);
                return string({text, length}, false);
            }
            case LUA_TTABLE:
                return table(idx, depth);
            case LUA_TLIGHTUSERDATA:
                if (is_null(L_, idx)) {
                    return writer_.Null();
                }
                [[fallthrough]];
            default:
                return fail(fmt::format("cannot encode a {}", luaL_typename(L_, idx)));
        }
    }

    [[nodiscard]] std::string_view text() const { return {buffer_.GetString(), buffer_.GetSize()}; }
    [[nodiscard]] const std::string& error() const { return error_; }

  private:
    bool fail(std::string what) {
        if (error_.empty()) {
            error_ = std::move(what);
        }
        return false;
    }

    bool number(double n) {
        if (!std::isfinite(n)) {
            return fail("cannot encode nan or inf");
        }
        // An exact integer is written as one: no exponent and no ".0".
        if (std::floor(n) == n && std::fabs(n) < kExactIntegerLimit) {
            return writer_.Int64(static_cast<std::int64_t>(n));
        }
        return writer_.Double(n);
    }

    bool string(std::string_view s, bool is_key) {
        const std::string_view clean = valid_utf8(s, scratch_);
        if (clean.size() > std::numeric_limits<rapidjson::SizeType>::max()) {
            return fail("string too long");
        }
        const auto length = static_cast<rapidjson::SizeType>(clean.size());
        return is_key ? writer_.Key(clean.data(), length) : writer_.String(clean.data(), length);
    }

    bool table(int idx, std::size_t depth) {
        if (depth >= kMaxDepth) {
            return fail("nesting too deep (a table that contains itself?)");
        }
        luaL_checkstack(L_, 4, "json nesting");

        // The length is a border, not a count: {1, 2, nil, 4, x = 5} may have length 4 and four keys. So a table
        // is an array only when every key is an integer in 1..length and there are length of them.
        const auto length = static_cast<std::size_t>(lua_objlen(L_, idx));
        std::size_t count = 0;
        std::size_t indices = 0;             // keys that are integers in 1..length
        std::vector<std::string_view> keys;  // views into strings the table keeps alive
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
                return fail("a json.array may only have the keys 1..n");
            }
            writer_.StartArray();
            for (std::size_t i = 1; i <= length; ++i) {
                lua_rawgeti(L_, idx, static_cast<int>(i));
                const bool ok = value(lua_gettop(L_), depth + 1);
                lua_pop(L_, 1);
                if (!ok) {
                    return false;
                }
            }
            return writer_.EndArray();
        }

        if (keys.size() != count) {
            return fail("a table is neither an array (keys 1..n) nor an object (string keys)");
        }
        std::sort(keys.begin(), keys.end());  // equal values always produce the same bytes
        writer_.StartObject();
        for (const std::string_view key : keys) {
            if (!string(key, true)) {
                return false;
            }
            lua_pushlstring(L_, key.data(), key.size());
            lua_rawget(L_, idx);
            const bool ok = value(lua_gettop(L_), depth + 1);
            lua_pop(L_, 1);
            if (!ok) {
                return false;
            }
        }
        return writer_.EndObject();
    }

    lua_State* L_;
    rapidjson::StringBuffer buffer_;
    rapidjson::Writer<rapidjson::StringBuffer> writer_;
    std::string scratch_;
    std::string error_;
};

int l_encode(lua_State* L) {
    luaL_checkany(L, 1);
    lua_settop(L, 1);
    Encoder encoder(L);
    if (!encoder.value(1, 0)) {
        luaL_error(L, "json.encode: %s", encoder.error().c_str());
    }
    const std::string_view text = encoder.text();
    lua_pushlstring(L, text.data(), text.size());
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
