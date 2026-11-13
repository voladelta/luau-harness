//! Bounded data-only copies for checkpoints, controllers, batches, and isolated packages.
use mlua::{Lua, Table, Value};
use serde_json::{Map, Number};
use std::collections::HashSet;

// Only native-created array tables carry this opaque marker. __metatable hides it from Luau,
// and both the tag table and its registry entry remain owned by this Lua instance.
static ARRAY_TAG: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
fn array_marker() -> mlua::LightUserData {
    mlua::LightUserData((&ARRAY_TAG as *const std::sync::atomic::AtomicU8).cast_mut().cast())
}
fn array_metatable(lua: &Lua) -> mlua::Result<Table> {
    if let Ok(table) = lua.named_registry_value::<Table>("hx.native_array") { return Ok(table); }
    let table = lua.create_table()?;
    table.raw_set("__hx_array", array_marker())?;
    table.raw_set("__metatable", "native array")?;
    table.set_readonly(true);
    lua.set_named_registry_value("hx.native_array", table.clone())?;
    Ok(table)
}

/// Also the cap on controller state, which holds the conversation and its images: std/compact derives its image budget
/// from it (`hx.limits.STATE_BYTES` in the controller).
pub(crate) const MAX_BYTES: usize = 16 << 20;
pub(crate) const MAX_NODES: usize = 262_144;
pub(crate) const MAX_DEPTH: usize = 64;
#[derive(Default)]
pub(crate) struct Limit { bytes: usize, nodes: usize }
impl Limit {
    pub(crate) fn charge(&mut self, bytes: usize, nodes: usize) -> mlua::Result<()> {
        self.bytes = self.bytes.saturating_add(bytes);
        self.nodes = self.nodes.saturating_add(nodes);
        if self.bytes > MAX_BYTES || self.nodes > MAX_NODES {
            return Err(mlua::Error::runtime("data exceeds 16 MiB or 262144 nodes"));
        }
        Ok(())
    }
    pub(crate) fn string(&mut self, s: &str) -> mlua::Result<()> {
        // Charged before allocating a Rust copy (one charge: images are megabytes).
        self.charge(string_bytes(s), 0)
    }
}

/// Conservative JSON escaping bound for a string.
fn string_bytes(s: &str) -> usize {
    2 + s.bytes().map(|b| if b < 32 { 6 } else if b == b'"' || b == b'\\' { 2 } else { 1 }).sum::<usize>()
}

/// The bytes `validate` charges for one node, its children excluded; each node also counts once toward MAX_NODES.
fn node_bytes(value: &serde_json::Value) -> usize {
    1 + match value {
        serde_json::Value::String(s) => string_bytes(s),
        serde_json::Value::Array(_) => 2,
        serde_json::Value::Object(o) => 2 + o.keys().map(|k| string_bytes(k)).sum::<usize>(),
        _ => 24,
    }
}

/// What `validate` charges for `value` as (bytes, nodes), plus its key's string when it is an object member. The durable
/// store keeps its store-wide total with this, per entry, so the total and `validate` cannot disagree.
pub(crate) fn charge(key: Option<&str>, value: &serde_json::Value) -> (usize, usize) {
    fn walk(v: &serde_json::Value, total: &mut (usize, usize)) {
        total.0 += node_bytes(v);
        total.1 += 1;
        match v { serde_json::Value::Array(a) => a.iter().for_each(|v| walk(v, total)), serde_json::Value::Object(o) => o.values().for_each(|v| walk(v, total)), _ => {} }
    }
    let mut total = (key.map_or(0, string_bytes), 0);
    walk(value, &mut total);
    total
}

/// The stored-state reference key (`cas::MARKER`) is reserved: data that could name a blob could, once checkpointed,
/// be expanded into another blob of the run store on resume.
fn reserved() -> mlua::Error { mlua::Error::runtime(format!("the key {:?} is reserved for stored state", crate::cas::MARKER)) }

pub fn copy_out(value: Value) -> mlua::Result<serde_json::Value> {
    fn visit(value: Value, stack: &mut HashSet<usize>, depth: usize, limit: &mut Limit) -> mlua::Result<serde_json::Value> {
        if depth > MAX_DEPTH { return Err(mlua::Error::runtime(format!("data nesting exceeds {MAX_DEPTH} levels"))); }
        limit.charge(1, 1)?;
        let bad = || mlua::Error::runtime("expected data only: finite numbers, strings, booleans, null, arrays or string-keyed objects; functions, handles, cycles and metatables are forbidden");
        Ok(match value {
            Value::Nil => { limit.charge(4, 0)?; serde_json::Value::Null },
            Value::LightUserData(p) if p.0.is_null() => { limit.charge(4, 0)?; serde_json::Value::Null },
            Value::Boolean(v) => { limit.charge(5, 0)?; v.into() },
            Value::Integer(v) => { limit.charge(24, 0)?; v.into() },
            Value::Number(v) => { limit.charge(24, 0)?; serde_json::Value::Number(Number::from_f64(v).ok_or_else(bad)?) },
            Value::String(v) => { let s = v.to_str()?; limit.string(&s)?; serde_json::Value::String(s.to_string()) },
            Value::Table(t) => {
                let tagged_array = match t.metatable() {
                    Some(mt) => match mt.raw_get::<Value>("__hx_array")? {
                        Value::LightUserData(marker) if marker == array_marker() => true,
                        _ => return Err(bad()),
                    },
                    None => false,
                };
                let ptr = t.to_pointer() as usize;
                if !stack.insert(ptr) { return Err(bad()); }
                let mut len = 0;
                let mut numeric = true;
                // Do not collect all pairs before checking the traversal budget.
                for pair in t.pairs::<Value, Value>() {
                    let (k, _) = pair?;
                    len += 1;
                    if len > MAX_NODES - limit.nodes { return Err(mlua::Error::runtime("data exceeds 262144 nodes")); }
                    numeric &= matches!(k, Value::Integer(i) if i > 0);
                }
                limit.charge(2, 0)?;
                let out = if tagged_array || (len > 0 && numeric) {
                    if !numeric { return Err(bad()); }
                    for pair in t.pairs::<usize, Value>() { if pair?.0 > len { return Err(bad()); } }
                    let mut items = Vec::new();
                    for i in 1..=len { items.push(visit(t.raw_get(i)?, stack, depth + 1, limit)?); }
                    serde_json::Value::Array(items)
                } else {
                    let mut fields = Map::new();
                    for pair in t.pairs::<Value, Value>() {
                        let (key, value) = pair?;
                        let Value::String(key) = key else { return Err(bad()); };
                        let key = key.to_str()?;
                        if *key == *crate::cas::MARKER { return Err(reserved()); }
                        limit.string(&key)?;
                        fields.insert(key.to_string(), visit(value, stack, depth + 1, limit)?);
                    }
                    serde_json::Value::Object(fields)
                };
                stack.remove(&ptr);
                out
            }
            _ => return Err(bad()),
        })
    }
    visit(value, &mut HashSet::new(), 0, &mut Limit::default())
}

fn validate_with_limit(value: &serde_json::Value, depth: usize, limit: &mut Limit) -> mlua::Result<()> {
        if depth > MAX_DEPTH { return Err(mlua::Error::runtime(format!("data nesting exceeds {MAX_DEPTH} levels"))); }
        limit.charge(node_bytes(value), 1)?;
        match value {
            serde_json::Value::Array(v) => { for v in v { validate_with_limit(v, depth + 1, limit)?; } },
            serde_json::Value::Object(v) => {
                if v.contains_key(crate::cas::MARKER) { return Err(reserved()); }
                for v in v.values() { validate_with_limit(v, depth + 1, limit)?; }
            },
            _ => {},
        }
        Ok(())
}

pub fn validate(value: &serde_json::Value) -> mlua::Result<()> {
    validate_with_limit(value, 0, &mut Limit::default())
}

pub fn copy_many(values: impl IntoIterator<Item = Value>) -> mlua::Result<Vec<serde_json::Value>> {
    let mut limit = Limit::default();
    let mut out = Vec::new();
    for value in values {
        let value = copy_out(value)?;
        validate_with_limit(&value, 0, &mut limit)?;
        out.push(value);
    }
    Ok(out)
}

pub fn copy_in(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
    fn visit(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
        Ok(match value {
            serde_json::Value::Null => Value::NULL,
            serde_json::Value::Bool(v) => Value::Boolean(*v),
            serde_json::Value::Number(v) => match v.as_i64() { Some(v) => Value::Integer(v), None => Value::Number(v.as_f64().unwrap()) },
            serde_json::Value::String(v) => Value::String(lua.create_string(v)?),
            serde_json::Value::Array(v) => {
                let t = lua.create_table()?;
                for (i, value) in v.iter().enumerate() { t.raw_set(i + 1, visit(lua, value)?)?; }
                t.set_metatable(Some(array_metatable(lua)?))?;
                Value::Table(t)
            }
            serde_json::Value::Object(v) => {
                let t: Table = lua.create_table()?;
                for (key, value) in v { t.raw_set(key.as_str(), visit(lua, value)?)?; }
                Value::Table(t)
            }
        })
    }
    validate(value)?;
    visit(lua, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boundaries_reject_hidden_state_and_copy_aliases() {
        let lua = Lua::new();
        for code in ["return function() end", "local t = {}; t.t=t; return t", "return setmetatable({}, {})", "return { [1]=1, x=2 }", "return 0/0"] {
            assert!(copy_out(lua.load(code).eval().unwrap()).is_err(), "{code}");
        }
        let source: Table = lua.load("return {x={n=1}, list={2,3}} ").eval().unwrap();
        let data = copy_out(Value::Table(source.clone())).unwrap();
        source.get::<Table>("x").unwrap().set("n", 9).unwrap();
        assert_eq!(data["x"]["n"], 1);
        assert_eq!(copy_out(copy_in(&lua, &data).unwrap()).unwrap(), data);
    }
    #[test]
    fn alias_expansion_is_bounded_and_native_arrays_are_plain() {
        let lua = Lua::new();
        let dag = lua.load("local t={x=1}; for i=1,30 do t={a=t,b=t} end; return t").eval().unwrap();
        assert!(copy_out(dag).unwrap_err().to_string().contains("data exceeds"));
        let json = serde_json::json!({"nested":[[1,2], [3], []], "empty":[]});
        assert_eq!(copy_out(copy_in(&lua, &json).unwrap()).unwrap(), json);
        let array = copy_in(&lua, &serde_json::json!([])).unwrap();
        lua.globals().set("array", array).unwrap();
        assert_eq!(lua.load("return getmetatable(array)").eval::<String>().unwrap(), "native array");
        assert!(validate(&serde_json::json!({"large":"x".repeat(MAX_BYTES)})).is_err());
    }
    #[test]
    fn stored_state_references_cannot_be_forged_from_data() {
        let lua = Lua::new();
        let sha = "0".repeat(64);
        let forged = lua.load(format!("return {{ messages = {{ {{ ['$hx.blob'] = '{sha}' }} }} }}")).eval().unwrap();
        assert!(copy_out(forged).unwrap_err().to_string().contains("reserved"));
        let nested = serde_json::json!({"a":[{"b":{crate::cas::MARKER: sha}}]});
        assert!(validate(&nested).unwrap_err().to_string().contains("reserved"));
        assert!(copy_in(&lua, &nested).is_err());
        assert!(validate(&serde_json::json!({"$hx.blob2": 1, "hx.blob": 2})).is_ok());
    }
    #[test]
    fn charge_is_what_validate_counts() {
        let value = serde_json::json!({"s":"a\"\u{1}é","n":[1, 2.5, null, true, {}, []],"o":{"k":"v"}});
        let (bytes, nodes) = charge(None, &value);
        let mut limit = Limit::default();
        validate_with_limit(&value, 0, &mut limit).unwrap();
        assert_eq!((limit.bytes, limit.nodes), (bytes, nodes));
        assert_eq!(charge(Some("key"), &value), (bytes + 5, nodes));
    }
}
