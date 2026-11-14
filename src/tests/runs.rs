//! Whole runs under the supervisor: what chunks, packages, controllers and descendants may do, and what the journal
//! keeps of them.
use super::scripted::{Scripted, luau, text};
use super::{Launch, Workspace, has_replied, resume, run, task_of};
use serde_json::{Value, json};
use std::sync::Arc;

/// The model answers `first` to a conversation's first request and `then` to every later one.
fn first_then(first: &'static str, then: &'static str) -> Arc<Scripted> {
    Scripted::by(move |_, index| if index == 0 { luau(first) } else { text(then) })
}

/// A parent whose one chunk starts two read-only children in a sealed batch and checks they finished.
const TWO_CHILDREN: &str = r#"local r=batch({{op="agent",args={"CHILD ONE"}},{op="agent",args={"CHILD TWO"}}})
for _, c in r do assert(c.ok and c.value.finished, c.error) end
print("CHILDREN_OK")"#;

#[tokio::test]
async fn a_resumed_run_keeps_its_frozen_config_and_answer() {
    let ws = Workspace::new();
    let model = first_then(r#"durable.set("keep",{v=9}); print("ok")"#, "done");
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["finished"], true);
    // workspace edits after launch neither run nor change the resumed run
    ws.write(
        ".harness/init.luau",
        r#"error("new config must not execute")"#,
    );
    ws.write(".harness/policy.toml", "[approval]\nallow=[\"git push\"]\n");
    let resumed = resume(&ws, &model, &ran.log).await;
    assert_eq!(resumed.report()["finished"], true);
    assert_eq!(resumed.report()["final_answer"], "done");
    assert_eq!(model.bodies().len(), 2);
}

#[tokio::test]
async fn packages_are_isolated_stateless_and_frozen_at_launch() {
    let ws = Workspace::new();
    ws.write(
        ".harness/pkgs/probe/hx.toml",
        "[package]\nversion=\"1\"\nkind=\"tool\"\n[capabilities]\nfs=[\"read\"]\n",
    );
    ws.write(
        ".harness/pkgs/probe/init.luau",
        r#"local n=0
return {
 data={10,20},
 count=function() n+=1; return n end,
 echo=function(t) t.n=8; return t end,
 write=function() return write("escape.txt","bad") end,
 import=function() return require("std/prompt") end,
 closure=function() return function() end end,
 read=function() return read("task.md") end,
}"#,
    );
    ws.write("task.md", "Perform the scripted test task.");
    let root = ws.root.clone();
    let model = Scripted::by(move |_, index| match index {
        0 => luau(
            r#"assert(tools.probe.count()==1 and tools.probe.count()==1)
assert(tools.probe.data[1]==10 and tools.probe.data[2]==20 and tools.probe.data["1"]==nil)
local t={n=1}; assert(tools.probe.echo(t).n==8 and t.n==1)
assert(not pcall(tools.probe.write)); assert(not pcall(tools.probe.import)); assert(not pcall(tools.probe.closure))
assert(not pcall(tools.probe.echo,function() end)); assert(tools.probe.read():find("scripted",1,true))
print("ISOLATION_OK")"#,
        ),
        1 => {
            // a package edited mid-run is not reloaded
            std::fs::write(
                root.join(".harness/pkgs/probe/init.luau"),
                r#"return {count=function() write("escape.txt","bad");return 99 end}"#,
            )
            .unwrap();
            luau(
                r#"assert(tools.probe.count()==1); assert(refresh_extensions==nil); print("FROZEN_OK")"#,
            )
        }
        _ => text("done"),
    });
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["finished"], true);
    assert_eq!(ran.report()["luau_errors"], 0);
    assert!(!ws.root.join("escape.txt").exists());
}

#[tokio::test]
async fn sealed_batches_refuse_mutations_and_keep_result_order() {
    let ws = Workspace::new();
    ws.write("task.md", "Perform the scripted test task.");
    let model = first_then(
        r#"local t={{op="read",args={"task.md"}},{op="search",args={"scripted",{glob="*.md"}}}}
local r=batch(t); assert(r[1].ok and r[1].value:find("scripted",1,true)); assert(r[2].ok and #r[2].value>0)
assert(not pcall(batch,{{op="read",args={"task.md"}},{op="write",args={"escape","bad"}}}))
assert(not pcall(batch,{function() end})); assert(parallel==nil); print("BATCH_OK")"#,
        "done",
    );
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["luau_errors"], 0);
    let reads = ran
        .records("call")
        .into_iter()
        .filter(|r| r["tool"] == "read")
        .count();
    assert_eq!(reads, 1);
}

#[tokio::test]
async fn invalid_agent_options_are_refused_before_dispatch() {
    let ws = Workspace::new();
    ws.write(
        ".harness/pkgs/child/hx.toml",
        "[package]\nversion=\"1\"\nkind=\"tool\"\n[capabilities]\nllm=[\"agent\"]\n",
    );
    ws.write(
        ".harness/pkgs/child/init.luau",
        r#"return {run=function(opts) return agent("child",opts) end}"#,
    );
    ws.write("task.md", "Perform the scripted test task.");
    let model = first_then(
        r#"for _, opts in {{effort="turbo"}, {model=7}, {max_turns="many"}, {max_turns=-1}} do
    assert(not pcall(agent,"child",opts))
    assert(not pcall(tools.child.run,opts))
    assert(not pcall(batch,{{op="read",args={"task.md"}},{op="agent",args={"child",opts}}}))
end
print("OPTIONS_VALIDATED")"#,
        "done",
    );
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["finished"], true);
    assert_eq!(ran.report()["luau_errors"], 0);
    assert_eq!(model.bodies().len(), 2);
    assert!(
        !ran.records
            .iter()
            .any(|r| r["tool"] == "read" || r["tool"] == "agent")
    );
}

#[tokio::test]
async fn a_package_agent_returns_what_agent_returns_and_carries_its_options() {
    let ws = Workspace::new();
    ws.write(
        ".harness/pkgs/child/hx.toml",
        "[package]\nversion=\"1\"\nkind=\"tool\"\n[capabilities]\nllm=[\"agent\"]\n",
    );
    ws.write(
        ".harness/pkgs/child/init.luau",
        r#"return {run=function() return agent("PACKAGE CHILD",{model="mock",effort="medium",max_turns=2}) end}"#,
    );
    let model = Scripted::by(|body, index| {
        if task_of(body).starts_with("PACKAGE CHILD") {
            return if has_replied(body) {
                text("child done")
            } else {
                luau(r#"print("CHILD")"#)
            };
        }
        if index == 0 {
            luau(
                r#"local r=tools.child.run(); durable.set("report",r); assert(r.finished); print("ARRAY_OK")"#,
            )
        } else {
            text("done")
        }
    });
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["finished"], true);
    assert_eq!(ran.report()["luau_errors"], 0);
    let requests = model.requests();
    assert_eq!(requests.len(), 4);
    let child = requests
        .iter()
        .find(|r| task_of(&r.body).starts_with("PACKAGE CHILD"))
        .unwrap();
    assert_eq!(child.cfg.reasoning.as_deref(), Some("medium"));
    let receipt = ran
        .records("call")
        .into_iter()
        .find(|r| r["tool"] == "agent")
        .unwrap();
    assert_eq!(
        (&receipt["args"]["model"], &receipt["args"]["effort"]),
        (&json!("mock"), &json!("medium"))
    );
    let checkpoint = ran
        .records("checkpoint")
        .into_iter()
        .rev()
        .find(|r| r["lane"] == "root")
        .unwrap();
    let blob = ran
        .log
        .parent()
        .unwrap()
        .join("blobs")
        .join(checkpoint["blob"].as_str().unwrap());
    let saved: Value = serde_json::from_slice(&std::fs::read(blob).unwrap()).unwrap();
    assert_eq!(
        saved["durable"]["report"],
        json!({"answer": "child done", "finished": true, "turns": 2})
    );
}

#[tokio::test]
async fn durable_data_is_bounded() {
    let ws = Workspace::new();
    let model = first_then(
        r#"local t={x=1}; for i=1,30 do t={a=t,b=t} end
assert(not pcall(durable.set,"dag",t)); assert(durable.get("dag")==nil)
local s=string.rep("x",1024*1024)
for i=1,15 do durable.set(tostring(i),s) end
assert(not pcall(durable.set,"16",s)); assert(durable.get("16")==nil)
assert(#durable.get("1")==#s); print("BOUNDED_OK")"#,
        "done",
    );
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["finished"], true);
    assert_eq!(ran.report()["luau_errors"], 0);
}

#[tokio::test]
async fn invalid_controller_commands_fail_before_any_dispatch_or_checkpoint() {
    for command in [
        r#"{kind="finish",report="oops"}"#,
        r#"{kind="model",request={system=8,messages="oops"}}"#,
        r#"{kind="run",code={}}"#,
    ] {
        let ws = Workspace::new();
        ws.write(
            ".harness/override/std/loop.luau",
            &format!(
                r#"return {{init=function() return {{}} end, step=function(s,e) return {{phase="finished"}},{command} end}}"#
            ),
        );
        let model = Scripted::replies(vec![text("unused")]);
        let ran = run(&ws, &model, Launch::default()).await;
        ran.error();
        assert!(model.bodies().is_empty(), "{command}");
        assert!(ran.records("checkpoint").is_empty(), "{command}");
    }
}

#[tokio::test]
async fn the_controller_has_no_effect_authority() {
    let ws = Workspace::new();
    ws.write(".harness/init.luau", r#"write("escape.txt","bad")"#);
    let model = Scripted::replies(vec![text("unused")]);
    let ran = run(&ws, &model, Launch::default()).await;
    ran.error();
    assert!(model.bodies().is_empty());
    assert!(!ws.root.join("escape.txt").exists());
}

#[tokio::test]
async fn read_only_children_cannot_regain_package_authority() {
    let ws = Workspace::new();
    ws.write(
        ".harness/pkgs/writer/hx.toml",
        "[package]\nversion=\"1\"\nkind=\"tool\"\n[capabilities]\nfs=[\"write\"]\n",
    );
    ws.write(
        ".harness/pkgs/writer/init.luau",
        r#"return {run=function() return write("escape.txt","bad") end}"#,
    );
    // a batch agent's value is agent()'s {answer, finished, turns}; a child answers "child done" only when its
    // read-only assertions all held
    let model = Scripted::by(|body, _| {
        let first = !has_replied(body);
        if task_of(body).starts_with("CHILD") {
            return if first {
                luau(
                    r#"assert(not pcall(write,"escape.txt","bad")); assert(not pcall(sh,"echo bad > escape.txt"))
assert(not pcall(tools.writer.run)); print("READONLY" .. "_HELD")"#,
                )
            } else if super::mentions(body, "READONLY_HELD") {
                text("child done")
            } else {
                text("child failed")
            };
        }
        if first {
            luau(&TWO_CHILDREN.replace(
                "c.value.finished, c.error",
                r#"c.value.finished and c.value.answer=="child done", c.error or c.value.answer"#,
            ))
        } else {
            text("parent done")
        }
    });
    let ran = run(&ws, &model, Launch::default()).await;
    let report = ran.report();
    assert_eq!(
        (report["finished"].clone(), report["luau_errors"].clone()),
        (json!(true), json!(0))
    );
    assert_eq!(report["llm_calls"], 6);
    assert_eq!(model.bodies().len(), 6);
    assert!(!ws.root.join("escape.txt").exists());
}

#[tokio::test(start_paused = true)]
async fn concurrent_children_stop_together_at_the_token_cap() {
    let ws = Workspace::new();
    let model = Scripted::by(|_, index| {
        if index == 0 {
            luau(TWO_CHILDREN)
        } else {
            luau(r#"print("child")"#).after(0.15)
        }
    });
    let ran = run(&ws, &model, Launch::max_tokens(30)).await;
    let report = ran.report();
    assert_eq!(report["finished"], false);
    assert!(
        report["stopped"]
            .as_str()
            .unwrap()
            .contains("budget_exceeded"),
        "{report}"
    );
    assert_eq!(
        (model.bodies().len(), report["llm_calls"].clone()),
        (2, json!(2))
    );
    assert_eq!(report["budget"]["tokens"], 30);
}

#[tokio::test(start_paused = true)]
async fn concurrent_children_queue_under_a_token_cap_instead_of_failing() {
    // every request reserves more than 200 tokens (its bytes plus the output cap), so one runs at a time
    let ws = Workspace::new();
    let model = Scripted::by(|body, _| {
        let first = !has_replied(body);
        if task_of(body).starts_with("CHILD") {
            let reply = if first {
                luau(r#"print("child")"#)
            } else {
                text("child done")
            };
            return reply.after(0.15);
        }
        if first {
            luau(TWO_CHILDREN)
        } else {
            text("parent done")
        }
    });
    let ran = run(&ws, &model, Launch::max_tokens(200)).await;
    let report = ran.report();
    assert_eq!(
        (report["finished"].clone(), report["luau_errors"].clone()),
        (json!(true), json!(0))
    );
    assert_eq!(model.bodies().len(), 6);
    assert_eq!(report["budget"]["reserved_tokens"], 0);
}

#[tokio::test]
async fn childrens_requests_run_side_by_side_under_a_cap_that_fits_both() {
    let ws = Workspace::new();
    // each child's first request is answered only once the other's is in flight too
    let both = Arc::new(tokio::sync::Barrier::new(2));
    let model = Scripted::by(move |body, _| {
        let first = !has_replied(body);
        if task_of(body).starts_with("CHILD") {
            return if first {
                luau(r#"print("child")"#).together(&both)
            } else {
                text("child done")
            };
        }
        if first {
            luau(TWO_CHILDREN)
        } else {
            text("parent done")
        }
    });
    let ran = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run(&ws, &model, Launch::max_tokens(1_000_000)),
    )
    .await
    .expect("both children's requests are in flight at once");
    let report = ran.report();
    assert_eq!(
        (report["finished"].clone(), report["luau_errors"].clone()),
        (json!(true), json!(0))
    );
    assert_eq!(model.bodies().len(), 6);
}
