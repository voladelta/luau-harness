//! Finishing a run: the before_finish hook, the opt-in finish review, other hooks and subagents.
use super::scripted::{Scripted, luau, text};
use super::{Launch, Workspace, run};

#[tokio::test]
async fn a_hook_denies_a_chunk_and_a_subagent_answers_with_usage_shared() {
    let ws = Workspace::new().init(
        r#"require("std/hooks").on("before_run", function(p)
	if p.code:find("rm -rf", 1, true) then return { deny = "no rm -rf here" } end
end)"#,
    );
    ws.write("readme.txt", "hi\n");
    let model = Scripted::replies(vec![
        luau(r#"sh("rm -rf /tmp/zzz-hx")"#),
        luau("local r = agent(\"sub\")\nprint(\"sub said: \" .. r.answer)"),
        luau("print(1)"),
        text("SUB-ANSWER"),
        text("All done."),
    ]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(
        ran.trace.matches("no rm -rf here").count(),
        1,
        "{}",
        ran.trace
    );
    assert_eq!(ran.trace.matches("sub said: SUB-ANSWER").count(), 1);
    assert_eq!(ran.report()["llm_calls"], 5);
    assert_eq!(ran.report()["final_answer"], "All done.");
}

#[tokio::test]
async fn before_finish_can_send_the_model_back_once() {
    let ws = Workspace::new().init(
        r#"local hooks = require("std/hooks")
hooks.on("before_finish", function(p)
	if hooks.state.asked then return { action = "accept" } end
	hooks.state.asked = true
	return { action = "continue", message = "Check once more." }
end)"#,
    );
    let model = Scripted::replies(vec![
        luau("print(1)"),
        text("First answer."),
        text("Checked: final answer."),
    ]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["llm_calls"], 3);
    assert_eq!(ran.report()["final_answer"], "Checked: final answer.");
    assert!(super::mentions(&model.bodies()[2], "Check once more."));
}

#[tokio::test]
async fn the_opt_in_finish_review_asks_exactly_once() {
    let ws = Workspace::new().init(r#"require("std/recheck").enabled = true"#);
    let model = Scripted::replies(vec![
        luau("print(1)"),
        text("First answer."),
        text("Checked: final answer."),
    ]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["llm_calls"], 3);
    assert_eq!(ran.report()["final_answer"], "Checked: final answer.");
    assert_eq!(
        ran.trace.matches("state the root cause you fixed").count(),
        1
    );
}

#[tokio::test]
async fn by_default_the_first_final_answer_is_accepted() {
    let ws = Workspace::new();
    // the defaults, not the suite's init file
    ws.write(".harness/init.luau", "");
    let model = Scripted::replies(vec![
        luau("print(1)"),
        text("First answer."),
        text("Checked: final answer."),
    ]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["llm_calls"], 2);
    assert_eq!(ran.report()["final_answer"], "First answer.");
    assert_eq!(
        ran.trace.matches("state the root cause you fixed").count(),
        0
    );
}

#[tokio::test]
async fn a_before_finish_hook_must_return_an_action() {
    for handler in [
        r#"function() return {deny="refused"} end"#,
        r#"function(p) p.action=nil; p.deny="refused" end"#,
    ] {
        let ws = Workspace::new().init(&format!(
            r#"require("std/hooks").on("before_finish",{handler})"#
        ));
        let model = Scripted::replies(vec![luau(r#"print("checked")"#), text("done")]);
        let ran = run(&ws, &model, Launch::default()).await;
        assert!(
            ran.error().contains("before_finish must return"),
            "{}",
            ran.error()
        );
        assert_eq!(model.bodies().len(), 2);
    }
}

#[tokio::test]
async fn a_failing_control_hook_prevents_dispatch() {
    let ws = Workspace::new()
        .init(r#"require("std/hooks").on("before_run",function() error("guard failed") end)"#);
    let model = Scripted::replies(vec![luau(r#"write("escape.txt","bad")"#)]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert!(
        ran.error().contains("control hook failed"),
        "{}",
        ran.error()
    );
    assert!(!ws.root.join("escape.txt").exists());
}
