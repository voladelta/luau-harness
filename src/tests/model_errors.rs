//! Model requests that fail: how each failure is billed, retried or waited out, and what the run does next
//! (`docs/runtime-v2.md`, "Dispatch and uncertainty").
use super::scripted::{Scripted, Step, luau, status, text};
use super::{Launch, Workspace, resume, run};
use serde_json::{Value, json};
use std::time::Duration;

/// `billing_uncertain` of each failed model request in the journal.
fn uncertain(records: &[Value]) -> Vec<Value> {
    records
        .iter()
        .filter(|r| r["kind"] == "llm" && r.get("error").is_some())
        .map(|r| r["billing_uncertain"].clone())
        .collect()
}

/// Gaps between consecutive requests, in seconds on tokio's clock.
fn gaps(model: &Scripted) -> Vec<f64> {
    let at: Vec<_> = model.requests().iter().map(|r| r.at).collect();
    at.windows(2).map(|w| (w[1] - w[0]).as_secs_f64()).collect()
}

#[tokio::test(start_paused = true)]
async fn rate_limits_are_waited_out_honouring_retry_after_and_release_reservations() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![
        status(429, "slow down").retry_after(5.0),
        status(429, "slow down"),
        status(429, "slow down"),
        luau("print(1)"),
        text("done"),
    ]);
    let ran = run(&ws, &model, Launch::max_tokens(1_000_000)).await;
    let report = ran.report();
    assert_eq!(report["finished"], true);
    assert_eq!(report["budget"]["reserved_tokens"], 0);
    assert_eq!(report["rate_limits"], 3);
    assert_eq!(uncertain(&ran.records), [false, false, false]);
    assert!(
        ran.records("billing").is_empty(),
        "nothing generated, nothing billed"
    );
    // Retry-After first, then the default ladder from where it stands: 4 s, 8 s
    assert_eq!(gaps(&model)[..3], [5.0, 4.0, 8.0]);
    let bodies = model.bodies();
    assert!(
        bodies[..4].iter().all(|b| *b == bodies[0]),
        "a retry resends the request unchanged"
    );
}

#[tokio::test(start_paused = true)]
async fn the_rate_limit_ladder_gives_up_after_eight_retries() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![status(429, "slow down").retry_after(120.0)]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert!(ran.error().contains("429"), "{}", ran.error());
    assert_eq!(model.bodies().len(), 9);
    // a Retry-After beyond 30 s is capped at 30 s
    assert_eq!(gaps(&model), [30.0; 8]);
}

#[tokio::test(start_paused = true)]
async fn a_retry_that_would_outlast_the_deadline_is_not_made() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![
        status(429, "slow down").retry_after(10.0),
        text("done"),
    ]);
    let ran = run(&ws, &model, Launch::max_wall(5.0)).await;
    assert!(ran.error().contains("429"), "{}", ran.error());
    assert_eq!(model.bodies().len(), 1);
}

#[tokio::test]
async fn a_definite_rejection_releases_its_reservation_and_is_not_retried() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![status(400, "bad request"), text("done")]);
    let ran = run(&ws, &model, Launch::max_tokens(1_000_000)).await;
    assert!(ran.error().contains("bad request"), "{}", ran.error());
    assert_eq!(model.bodies().len(), 1);
    assert_eq!(uncertain(&ran.records), [false]);
    assert!(ran.records("billing").is_empty());
    let books = crate::supervisor::Books::of(&ran.records).unwrap();
    assert!(books.reservations.is_empty(), "no billing is left unknown");
    // nothing is left reserved, so a resumed run is admitted and finishes
    let resumed = resume(&ws, &model, &ran.log).await;
    assert_eq!(resumed.report()["finished"], true);
    assert_eq!(resumed.report()["budget"]["reserved_tokens"], 0);
}

#[tokio::test(start_paused = true)]
async fn a_server_error_gets_one_new_request_and_every_attempt_is_charged() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![status(502, "upstream")]);
    let ran = run(&ws, &model, Launch::max_tokens(1_000_000)).await;
    assert!(ran.error().contains("upstream"), "{}", ran.error());
    assert_eq!(model.bodies().len(), 2);
    assert_eq!(uncertain(&ran.records), [true, true]);
    let failed: Vec<_> = ran.records("llm").iter().map(|r| r["of"].clone()).collect();
    let billed: Vec<_> = ran
        .records("billing")
        .iter()
        .map(|r| r["of"].clone())
        .collect();
    assert_eq!(failed, billed);
    assert_eq!(gaps(&model), [2.0]);
}

#[tokio::test(start_paused = true)]
async fn a_server_error_then_a_reply_finishes() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![status(503, "busy"), luau("print(1)"), text("done")]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["finished"], true);
    // a 503 refused the request before any generation
    assert_eq!(uncertain(&ran.records), [false]);
    assert!(ran.records("billing").is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_malformed_reply_gets_one_new_request() {
    let ws = Workspace::new();
    // a 200 the client cannot use: the provider reported an error in the body
    let model = Scripted::replies(vec![status(200, "malformed"), text("done")]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["final_answer"], "done");
    assert_eq!(uncertain(&ran.records), [true]);
    assert_eq!(ran.records("billing").len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_lost_response_is_charged_its_reservation_and_one_new_request_follows() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![Step::Lost]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert!(
        ran.error().contains("response stream ended"),
        "{}",
        ran.error()
    );
    let bodies = model.bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
    let intents: Vec<_> = ran
        .records("model_intent")
        .iter()
        .map(|r| (r["seq"].clone(), r["reserved_tokens"].clone()))
        .collect();
    let billed: Vec<_> = ran
        .records("billing")
        .iter()
        .filter(|r| r["source"] == "hx")
        .map(|r| (r["of"].clone(), r["tokens"].clone()))
        .collect();
    assert_eq!(intents, billed);
    assert_eq!(uncertain(&ran.records), [true, true]);
}

#[tokio::test(start_paused = true)]
async fn a_lost_response_then_a_reply_finishes() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![Step::Lost, luau("print(2)"), text("all done")]);
    let ran = run(&ws, &model, Launch::default()).await;
    let report = ran.report();
    assert_eq!(
        (report["finished"].clone(), report["turns"].clone()),
        (json!(true), json!(2))
    );
    assert_eq!(report["code_turns"], 1);
}

#[tokio::test(start_paused = true)]
async fn a_request_that_never_left_is_unbilled_and_sent_once_more() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![Step::NotSent, text("done")]);
    let ran = run(&ws, &model, Launch::default()).await;
    assert_eq!(ran.report()["final_answer"], "done");
    assert_eq!(uncertain(&ran.records), [false]);
    assert!(ran.records("billing").is_empty());
}

#[tokio::test]
async fn the_run_deadline_ends_a_model_call_that_never_answers() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![Step::Hang]);
    let started = std::time::Instant::now();
    let ran = run(&ws, &model, Launch::max_wall(0.5)).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    let report = ran.report();
    assert_eq!(report["finished"], false);
    assert!(
        report["stopped"]
            .as_str()
            .unwrap()
            .contains("absolute run deadline"),
        "{report}"
    );
    assert_eq!(model.bodies().len(), 1, "no retry past the deadline");
    // the request may have been generated and billed: it is charged
    assert_eq!(uncertain(&ran.records), [true]);
    assert_eq!(ran.records("billing").len(), 1);
    assert_eq!(report["budget"]["reserved_tokens"], 0);
}
