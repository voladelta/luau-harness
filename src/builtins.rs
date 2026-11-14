//! Bindings with no subsystem of their own: durable data, `agent`, and image viewing for the vision pack.
use crate::{
    effects::Ret,
    policy::Class,
    prims,
    vm::{Context, VIEW_MAX},
};
use anyhow::Result;
use mlua::{Table, Value};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// `durable.get(key)` and `durable.set(key, value)`: JSON-like data a completed chunk commits to the checkpoint.
pub fn install_durable(cx: &Context) -> Result<()> {
    let (lua, supervisor) = (cx.lua, cx.supervisor);
    let saved = cx
        .host
        .map(|h| h.durable.clone())
        .unwrap_or_else(|| supervisor.durable.clone());
    let durable = lua.create_table()?;
    let data = saved.clone();
    durable.set(
        "get",
        lua.create_function(
            move |lua, key: String| match data.lock().unwrap().get(&key) {
                Some(value) => crate::values::copy_in(lua, value),
                None => Ok(Value::Nil),
            },
        )?,
    )?;
    let stage_owner = supervisor.clone();
    let stage_epoch = supervisor.workflow_epoch();
    durable.set(
        "set",
        lua.create_function(move |_, (key, value): (String, Value)| {
            stage_owner
                .authorize_stage(stage_epoch, crate::supervisor::Tool::Read, Class::Read)
                .map_err(mlua::Error::external)?;
            let value = crate::values::copy_out(value)?;
            let mut saved = saved.lock().unwrap();
            fn store(
                saved: &mut serde_json::Value,
            ) -> &mut serde_json::Map<String, serde_json::Value> {
                saved
                    .as_object_mut()
                    .expect("the durable store is validated as an object when loaded")
            }
            let previous = store(&mut saved).insert(key.clone(), value);
            if let Err(e) = crate::values::validate(&saved) {
                match previous {
                    Some(previous) => store(&mut saved).insert(key, previous),
                    None => store(&mut saved).remove(&key),
                };
                return Err(e);
            }
            Ok(())
        })?,
    )?;
    durable.set_readonly(true);
    lua.globals().set("durable", durable)?;
    Ok(())
}

/// `agent(task, opts?)`, when the VM serves an agent: a subagent with this agent's authority.
pub fn install_agent(cx: &Context) -> Result<()> {
    let Some(h) = cx.host.cloned() else {
        return Ok(());
    };
    let afx = cx.fx.clone();
    cx.lua.globals().set(
        "agent",
        cx.lua
            .create_async_function(move |lua, (task, opts): (String, Option<Table>)| {
                let (h, afx) = (h.clone(), afx.clone());
                async move {
                    let options = crate::host::AgentOptions::from_lua(opts)?;
                    let child = h
                        .child(&options, afx.principal.clone())
                        .map_err(mlua::Error::external)?;
                    prims::agent(&afx, &lua, child, task, &options)
                        .await?
                        .into_lua(&lua)
                }
            })?,
    )?;
    Ok(())
}

/// `hx.view_image(path, max) -> { path, width, height, orig_width, orig_height, mime, data }` for the vision pack.
/// Record the live image identity once and return the same loaded bytes.
pub fn install_view(cx: &Context, hx: &Table) -> Result<()> {
    let vfx = cx.fx.clone();
    hx.set("view_image", cx.lua.create_async_function(move |lua, (path, max): (String, Option<u32>)| {
        let vfx = vfx.clone();
        async move {
            let max = max.unwrap_or(VIEW_MAX.0).clamp(VIEW_MAX.1, VIEW_MAX.2);
            let (pol, image_path) = (vfx.pol.clone(), path.clone());
            // The journal records the image's identity; the image itself comes back through `live`.
            let live: Arc<Mutex<Option<crate::prims::ViewImage>>> = Arc::default();
            let slot = live.clone();
            vfx.run(&lua, crate::supervisor::Tool::View, Class::Read, json!({"path": &path, "max": max}), || prims::blocking(move || {
                let v = crate::prims::load_view_image(&pol, &image_path, max).map_err(mlua::Error::runtime)?;
                let meta = json!({"sha256": v.sha256, "width": v.width, "height": v.height}).to_string();
                *slot.lock().unwrap() = Some(v);
                Ok(Ret::Str(meta))
            })).await?;
            let v = live.lock().unwrap().take().ok_or_else(|| mlua::Error::runtime("view executor returned no image"))?;
            let t = lua.create_table()?;
            t.set("path", path)?;
            t.set("width", v.width)?;
            t.set("height", v.height)?;
            t.set("orig_width", v.orig.0)?;
            t.set("orig_height", v.orig.1)?;
            t.set("mime", v.mime)?;
            t.set("data", v.data_b64)?;
            Ok(t)
        }
    })?)?;
    Ok(())
}
