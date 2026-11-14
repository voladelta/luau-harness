//! std/compact in the loop: threshold compaction, the replayed summarizer request, merged summaries, globals that
//! survive, summary validation, overflow compact-and-retry, and subagents.
//!
//! Window 40000, max output 1000: the threshold is min(38000, 40000 - 1000 - 16384) = 22616, so a reply reporting 23000
//! prompt tokens crosses it, and the summarizer prefix still fits under 40000 - 16384 = 23616.
use super::scripted::{Scripted, Step, luau, status, text};
use super::{Launch, Ran, Workspace, content, mentions, run};
use serde_json::Value;
use std::sync::Arc;

const TASK: &str = "TASKTEXT-SCRIPTED";

/// A `luau` call with about 2000 characters of padding, so a summary is smaller than what it replaces.
fn chunk(code: &str) -> Step {
    luau(&format!("{code} -- {}", "p".repeat(2000))).tokens(10)
}

/// A workspace that keeps as short a tail as it can.
fn workspace() -> Workspace {
    Workspace::new().init(r#"require("std/compact").keep_ratio = 0.001"#)
}

fn launch() -> Launch {
    Launch {
        task: TASK.into(),
        context_window: Some(40000),
        max_output: 1000,
        ..Launch::default()
    }
}

async fn compacting(ws: &Workspace, steps: Vec<Step>) -> (Ran, Arc<Scripted>) {
    let model = Scripted::replies(steps);
    (run(ws, &model, launch()).await, model)
}

fn messages(body: &Value) -> &Vec<Value> {
    body["messages"].as_array().expect("messages")
}

fn is_summarizer(body: &Value) -> bool {
    messages(body)
        .last()
        .is_some_and(|m| content(m).contains("Write a context checkpoint"))
}

fn summarizers(model: &Scripted) -> Vec<Value> {
    model.bodies().into_iter().filter(is_summarizer).collect()
}

#[tokio::test]
async fn below_the_threshold_the_pack_changes_no_request() {
    let steps = || vec![chunk("a=1"), chunk("b=2"), text("done")];
    let off = workspace().init(r#"require("std/compact").enabled = false"#);
    let (_, plain) = compacting(&off, steps()).await;
    let (_, packed) = compacting(&workspace(), steps()).await;
    assert_eq!(plain.bodies().len(), 3);
    assert_eq!(plain.bodies(), packed.bodies());
}

#[tokio::test]
async fn crossing_the_threshold_compacts_once_and_globals_survive() {
    let ws = workspace();
    let (ran, model) = compacting(
        &ws,
        vec![
            chunk("x = 42 -- TURN-ONE-MARKER"),
            chunk("y = 1").tokens(23000),
            text("SUMMARY-ONE"),
            luau("print(x)"),
            text("done"),
        ],
    )
    .await;
    assert_eq!(ran.report()["finished"], true);
    let bodies = model.bodies();
    assert_eq!((bodies.len(), summarizers(&model).len()), (5, 1));

    // the summarizer request is the previous agent request's prefix plus one user message
    let (agent, summarizer) = (&bodies[1], &bodies[2]);
    let n = messages(summarizer).len() - 1;
    assert!(is_summarizer(summarizer));
    assert_eq!(summarizer["system"], agent["system"]);
    assert_eq!(summarizer["tools"], agent["tools"]);
    assert_eq!(messages(summarizer)[..n], messages(agent)[..n]);
    assert_eq!(messages(summarizer)[n]["role"], "user");
    // it sees the task and the exact history; the kept tail is not replayed
    for needle in ["TURN-ONE-MARKER", TASK, "Dead Ends"] {
        assert!(mentions(summarizer, needle), "{needle}");
    }
    assert!(!mentions(summarizer, "y = 1"));

    // the next request: the task and the summary block, then the tail, whose tool results answer its calls
    let next = messages(&bodies[3]);
    let first = content(&next[0]);
    assert_eq!(next[0]["role"], "user");
    for needle in [TASK, "[compaction-summary]", "SUMMARY-ONE", "</summary>"] {
        assert!(first.contains(needle), "{needle}");
    }
    let ids: Vec<&Value> = next
        .iter()
        .flat_map(|m| m["calls"].as_array().into_iter().flatten())
        .map(|c| &c["id"])
        .collect();
    let results: Vec<&Value> = next
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| &m["id"])
        .collect();
    assert!(!ids.is_empty() && !results.is_empty() && results.iter().all(|id| ids.contains(id)));
    assert!(
        !mentions(&bodies[3], "TURN-ONE-MARKER"),
        "the dropped history is gone"
    );

    assert_eq!(ran.trace_lines("### COMPACT"), 1);
    // a global set before the compaction is readable after it
    let after = ran
        .trace
        .split("### CODE\nprint(x)\n")
        .nth(1)
        .expect("the chunk after");
    let result = after.split("### RESULT\n").nth(1).expect("its result");
    assert_eq!(result.lines().next(), Some("42"));
}

#[tokio::test]
async fn compaction_is_on_by_default_and_enabled_false_turns_it_off() {
    let steps = || {
        vec![
            chunk("x = 42"),
            chunk("y = 1").tokens(23000),
            text("SUMMARY-ONE"),
            luau("print(x)"),
            text("done"),
        ]
    };
    let (ran, model) = compacting(&workspace(), steps()).await;
    assert_eq!(ran.report()["finished"], true);
    assert_eq!(summarizers(&model).len(), 1);
    let off = workspace().init(r#"require("std/compact").enabled = false"#);
    let (_, model) = compacting(&off, steps()).await;
    assert_eq!(summarizers(&model).len(), 0);
    assert!(
        !model
            .bodies()
            .iter()
            .any(|b| mentions(b, "compaction-summary"))
    );
}

#[tokio::test]
async fn a_second_compaction_merges_the_first_summary() {
    let ws = workspace();
    let (ran, model) = compacting(
        &ws,
        vec![
            chunk("x = 42 -- TURN-ONE-MARKER"),
            chunk("y = 1").tokens(23000),
            text("SUMMARY-ONE"),
            chunk("print(x) -- TURN-THREE-MARKER"),
            chunk("z = 2 -- TURN-FOUR-MARKER").tokens(23000),
            text("SUMMARY-TWO"),
            text("done"),
        ],
    )
    .await;
    assert_eq!(ran.report()["finished"], true);
    let summaries = summarizers(&model);
    assert_eq!(summaries.len(), 2);
    let second = &summaries[1];
    assert!(mentions(second, "SUMMARY-ONE") && mentions(second, "earlier checkpoint"));
    assert!(!mentions(second, "TURN-ONE-MARKER") && mentions(second, "TURN-THREE-MARKER"));
    let bodies = model.bodies();
    let last = content(&messages(bodies.last().unwrap())[0]);
    assert!(last.contains("SUMMARY-TWO") && !last.contains("SUMMARY-ONE"));
}

#[tokio::test]
async fn a_failed_summarizer_leaves_the_history_and_two_failures_switch_the_pack_off() {
    let ws = workspace();
    let (ran, model) = compacting(
        &ws,
        vec![
            chunk("a=1"),
            chunk("b=2"),
            chunk("c=3").tokens(23000),
            text(""),
            chunk("d=4").tokens(23000),
            text(""),
            chunk("e=5").tokens(23000),
            text("done"),
        ],
    )
    .await;
    assert_eq!(ran.report()["finished"], true);
    assert!(!model.bodies().iter().any(|b| mentions(b, "<summary>")));
    assert_eq!(ran.trace_lines("### COMPACT FAILED"), 2);
    assert_eq!(summarizers(&model).len(), 2);
}

#[tokio::test]
async fn a_summarizer_api_error_counts_as_a_failure() {
    let ws = workspace();
    let (ran, _) = compacting(
        &ws,
        vec![
            chunk("a=1"),
            chunk("b=2"),
            chunk("c=3").tokens(23000),
            status(400, "boom"),
            chunk("d=4").tokens(23000),
            text("done"),
        ],
    )
    .await;
    assert_eq!(ran.report()["finished"], true);
    assert_eq!(ran.trace_lines("### COMPACT FAILED"), 1);
}

#[tokio::test]
async fn truncated_tool_calling_and_oversized_summaries_are_rejected() {
    let ws = workspace();
    let (ran, model) = compacting(
        &ws,
        vec![
            chunk("a=1"),
            chunk("b=2"),
            chunk("c=3").tokens(23000),
            text("SUMMARY-CUT").cut(),
            chunk("d=4").tokens(23000),
            luau("print(1)").with_text("x"),
            text("done"),
        ],
    )
    .await;
    assert_eq!(ran.report()["finished"], true);
    assert_eq!(ran.trace_lines("### COMPACT FAILED"), 2);
    assert!(!model.bodies().iter().any(|b| mentions(b, "<summary>")));

    let ws = workspace();
    let (ran, model) = compacting(
        &ws,
        vec![
            chunk("a=1"),
            chunk("b=2"),
            chunk("c=3").tokens(23000),
            text(&"L".repeat(40000)),
            text("done"),
        ],
    )
    .await;
    assert_eq!(ran.report()["finished"], true);
    assert_eq!(ran.trace_lines("### COMPACT FAILED"), 1);
    assert!(!model.bodies().iter().any(|b| mentions(b, "<summary>")));
}

/// A workspace whose summarizer headroom fits the scripted contexts, which are tiny (the default reserve, 16384,
/// would leave room for no prefix).
fn overflow_workspace() -> Workspace {
    workspace().init(r#"require("std/compact").reserve_tokens = 10"#)
}

const OVERFLOW: &str = "maximum context length exceeded";

#[tokio::test]
async fn a_context_overflow_compacts_once_and_retries_the_turn() {
    let ws = overflow_workspace();
    let (ran, model) = compacting(
        &ws,
        vec![
            chunk("a=1"),
            chunk("b=2"),
            status(400, OVERFLOW),
            text("SUMMARY-OV"),
            chunk("print(a) -- AFTER"),
            text("done"),
        ],
    )
    .await;
    let report = ran.report();
    assert_eq!(
        (report["finished"].as_bool(), report["turns"].as_u64()),
        (Some(true), Some(4))
    );
    let summaries = summarizers(&model);
    // it keeps as little as fits: here the newest exchange
    assert!(mentions(&summaries[0], "a=1") && !mentions(&summaries[0], "b=2"));
    let bodies = model.bodies();
    assert_eq!(bodies.len(), 6);
    assert!(
        !mentions(&bodies[2], "compaction-summary"),
        "the rejected request had no summary"
    );
    assert!(mentions(&bodies[4], "SUMMARY-OV"), "the retry carries it");
}

#[tokio::test]
async fn a_second_overflow_on_the_same_turn_fails_the_run() {
    let ws = overflow_workspace();
    let (ran, model) = compacting(
        &ws,
        vec![
            chunk("a=1"),
            chunk("b=2"),
            status(400, OVERFLOW),
            text("SUMMARY-OV"),
            status(400, "context_length_exceeded again"),
            text("never used"),
        ],
    )
    .await;
    assert!(
        ran.error()
            .contains("failed again after one compact-and-retry"),
        "{}",
        ran.error()
    );
    assert_eq!(model.bodies().len(), 5);
}

#[tokio::test]
async fn an_overflow_whose_compaction_fails_ends_the_run_with_the_api_error() {
    let steps = || {
        vec![
            chunk("a=1"),
            chunk("b=2"),
            status(400, OVERFLOW),
            text(""),
            text("never used"),
        ]
    };
    let (ran, _) = compacting(&overflow_workspace(), steps()).await;
    assert!(ran.error().contains(OVERFLOW), "{}", ran.error());
    // with the pack off, an overflow fails without a summarizer request
    let off = overflow_workspace().init(r#"require("std/compact").enabled = false"#);
    let (ran, model) = compacting(&off, steps()).await;
    assert!(ran.error().contains(OVERFLOW), "{}", ran.error());
    assert_eq!(model.bodies().len(), 3);
}

#[tokio::test]
async fn reasoning_heavy_turns_compact_most_of_the_history() {
    // real-model shape: reasoning, echoed back, dominates the context; a small keep ratio keeps a short tail
    let ws = Workspace::new().init(r#"require("std/compact").keep_ratio = 0.128"#);
    let reasoning = "r".repeat(6000);
    let mut steps: Vec<Step> = (1..=8)
        .map(|i| {
            chunk(&format!("t{i} = {i}"))
                .tokens(if i == 8 { 30000 } else { 3000 * i })
                .reasoning(&reasoning)
        })
        .collect();
    steps.extend([
        text("SUMMARY-R"),
        chunk("print(t1)").tokens(2000),
        text("done"),
    ]);
    let (ran, _) = compacting(&ws, steps).await;
    assert_eq!(ran.report()["finished"], true);
    let line = ran
        .trace
        .split("### COMPACT\n")
        .nth(1)
        .and_then(|s| s.lines().next());
    assert_eq!(
        line,
        Some("summarized 12 messages, kept 4, summary committed")
    );
}

#[tokio::test]
async fn a_subagent_compacts_its_own_history() {
    let ws = workspace();
    let (ran, model) = compacting(
        &ws,
        vec![
            luau("local r = agent(\"sub\")\nprint(r.answer)"),
            chunk("a=1 -- CHILD-MARK"),
            chunk("b=2 -- CHILD-MARK"),
            chunk("c=3 -- CHILD-MARK").tokens(23000),
            text("CHILD-SUMMARY"),
            text("child done"),
            text("parent done"),
        ],
    )
    .await;
    assert_eq!(ran.report()["finished"], true);
    let summaries = summarizers(&model);
    assert_eq!(summaries.len(), 1);
    assert!(mentions(&summaries[0], "CHILD-MARK") && !mentions(&summaries[0], "agent("));
}
