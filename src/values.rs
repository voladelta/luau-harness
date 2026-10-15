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

const MAX_BYTES: usize = 16 << 20;
const MAX_NODES: usize = 262_144;
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
        self.charge(2, 0)?;
        // Conservative JSON escaping bound, charged before allocating a Rust copy.
        for b in s.bytes() { self.charge(if b < 32 { 6 } else if b == b'"' || b == b'\\' { 2 } else { 1 }, 0)?; }
        Ok(())
    }
}

pub fn copy_out(value: Value) -> mlua::Result<serde_json::Value> {
    fn visit(value: Value, stack: &mut HashSet<usize>, depth: usize, limit: &mut Limit) -> mlua::Result<serde_json::Value> {
        if depth > 64 { return Err(mlua::Error::runtime("data nesting exceeds 64 levels")); }
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
        if depth > 64 { return Err(mlua::Error::runtime("data nesting exceeds 64 levels")); }
        limit.charge(1, 1)?;
        match value {
            serde_json::Value::String(s) => limit.string(s)?,
            serde_json::Value::Array(v) => { limit.charge(2, 0)?; for v in v { validate_with_limit(v, depth + 1, limit)?; } },
            serde_json::Value::Object(v) => { limit.charge(2, 0)?; for (k, v) in v { limit.string(k)?; validate_with_limit(v, depth + 1, limit)?; } },
            _ => limit.charge(24, 0)?,
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
}
