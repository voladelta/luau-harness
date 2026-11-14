//! The loop's turns: tool calls, their results, and the nudges for replies that ran nothing.
use super::scripted::{Scripted, luau, luau_calls, text};
use super::{Launch, Workspace, content, mentions, run};
use serde_json::{Value, json};

/// finished|final_answer|turns|code_turns|truncated_turns, as the llm suite printed it.
fn summary(report: &Value) -> String {
    [
        "finished",
        "final_answer",
        "turns",
        "code_turns",
        "truncated_turns",
    ]
    .iter()
    .map(|k| match &report[k] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
    .collect::<Vec<_>>()
    .join("|")
}

#[tokio::test]
async fn a_code_turn_then_an_answer_finishes_the_run() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![luau("print(7)"), text("all done")]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(summary(ran.report()), "true|all done|2|1|0");
    let bodies = model.bodies();
    assert_eq!(bodies.len(), 2);
    // the luau tool is declared, and the chunk's output answers its call
    assert!(
        bodies[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "luau")
    );
    let result = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("a tool result");
    assert_eq!(result["id"], "call_0_0");
    assert!(content(result).contains('7'), "{result}");
    assert_eq!(ran.report()["llm_calls"], 2);
    assert_eq!(ran.records("model_intent").len(), 2);
}

#[tokio::test]
async fn parallel_calls_in_one_reply_all_run_and_each_gets_a_result() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![
        luau_calls(&["print(1)", "print(2)"]),
        text("all done"),
    ]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(summary(ran.report()), "true|all done|2|2|0");
    let ids: Vec<Value> = model.bodies()[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["id"].clone())
        .collect();
    assert_eq!(ids, [json!("call_0_0"), json!("call_0_1")]);
}

#[tokio::test]
async fn invalid_tool_arguments_get_an_error_result_and_the_run_continues() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![
        text("").call("luau", "{\"code\": \"print(1"),
        luau("print(2)"),
        text("all done"),
    ]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(summary(ran.report()), "true|all done|3|1|1");
}

#[tokio::test]
async fn an_unknown_tool_gets_an_error_result_and_the_run_continues() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![
        text("").call("bash", "{}"),
        luau("print(2)"),
        text("all done"),
    ]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(summary(ran.report()), "true|all done|3|1|0");
}

#[tokio::test]
async fn plain_prose_after_a_code_turn_is_the_answer() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![luau("print(7)"), text("The fix is in place.")]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(summary(ran.report()), "true|The fix is in place.|2|1|0");
}

#[tokio::test]
async fn tool_call_markup_in_the_text_is_nudged_not_taken_as_the_answer() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![
        luau("print(7)"),
        text("part07: SECRET=614</｜DSML｜ parameter>"),
        luau("print(2)"),
        text("all done"),
    ]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(summary(ran.report()), "true|all done|4|2|1");
    assert!(mentions(&model.bodies()[2], "arrived as plain text"));
}

#[tokio::test]
async fn prose_before_any_code_is_nudged_to_start() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![text("I will look."), luau("print(7)"), text("done")]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(summary(ran.report()), "true|done|3|1|1");
    assert!(mentions(
        &model.bodies()[1],
        "You have not run any code yet"
    ));
}

#[tokio::test]
async fn a_cut_off_reply_is_nudged_to_retry_shorter() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![
        luau("print(7)"),
        text("half an ans").cut(),
        text("all done"),
    ]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(summary(ran.report()), "true|all done|3|1|1");
    let last = model.bodies()[2]["messages"].as_array().unwrap().clone();
    assert_eq!(content(&last[last.len() - 2]), "half an ans");
    assert!(content(&last[last.len() - 1]).starts_with("Your last reply was cut off or empty"));
}

#[tokio::test]
async fn an_empty_reply_is_resent_unchanged_not_nudged() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![luau("print(7)"), text(""), text("all done")]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(summary(ran.report()), "true|all done|3|1|1");
    let bodies = model.bodies();
    assert_eq!(bodies[1], bodies[2], "the resend is the same request");
    assert!(!mentions(&bodies[2], "cut off or empty"));
}

#[tokio::test]
async fn repeated_empty_replies_stop_the_run() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![luau("print(7)"), text("")]);
    let ran = run(&ws, &model, Launch::default()).await;
    let report = ran.report();
    assert_eq!(report["finished"], false);
    assert_eq!(
        report["stopped"],
        "protocol: repeated empty or truncated replies"
    );
}

#[tokio::test]
async fn max_turns_stops_a_run_that_keeps_calling_tools() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![luau("print(1)")]);
    let launch = Launch {
        max_turns: 3,
        ..Launch::default()
    };
    let ran = run(&ws, &model, launch).await;
    let report = ran.report();
    assert_eq!(report["finished"], false);
    assert_eq!(report["stopped"], "max_turns: 3");
    assert_eq!(
        (report["turns"].clone(), model.bodies().len()),
        (json!(3), 3)
    );
}

#[tokio::test]
async fn reasoning_is_echoed_back_on_the_next_turn() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![
        luau("print(7)").reasoning("REASONING-MARK"),
        text("all done"),
    ]);
    run(&ws, &model, Launch::default()).await.report();
    let bodies = model.bodies();
    assert!(mentions(&bodies[1], "REASONING-MARK"));
}
