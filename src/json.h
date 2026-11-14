#pragma once

struct lua_State;

namespace hx {

// Registers the global `json` table:
//
//   json.encode(value) -> string          raises on a value JSON cannot express
//   json.decode(text)  -> value | nil, error
//   json.array(table?) -> table           marks a table (or a new one) as an array, so an empty one encodes as []
//   json.null                             stands for null inside arrays and at the top level
//
// Objects are written with sorted keys, so equal values always encode to the same bytes. A null object member
// decodes to an absent key. An unmarked empty table encodes as {}. Strings that are not valid UTF-8 are encoded
// with U+FFFD in place of the bad bytes, because command output and file contents are not always text.
void open_json(lua_State* L);

} // namespace hx
