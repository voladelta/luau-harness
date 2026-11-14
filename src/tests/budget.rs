//! Run budgets: token admission, the wall-clock deadline, and descendants sharing both.
use super::scripted::{Scripted, luau, text};
use super::{Launch, Workspace, run};

/// Five code turns, then an answer; every reply costs 10 + 5 tokens.
fn five_chunks() -> std::sync::Arc<Scripted> {
    Scripted::replies(vec![
        luau("print(1)"),
        luau("print(2)"),
        luau("print(3)"),
        luau("print(4)"),
        luau("print(5)"),
        text("done"),
    ])
}

#[tokio::test]
async fn the_token_budget_stops_the_run_at_admission() {
    let ws = Workspace::new();
    let ran = run(&ws, &five_chunks(), Launch::max_tokens(40)).await;
    let report = ran.report();
    assert_eq!(report["llm_calls"], 3);
    assert!(
        report["stopped"]
            .as_str()
            .unwrap()
            .contains("token admission threshold")
    );
    assert_eq!(report["finished"], false);
    assert_eq!(ran.records("budget").len(), 1);
}

#[tokio::test]
async fn policy_toml_sets_the_budget_and_a_launch_flag_beats_it() {
    let ws = Workspace::new();
    ws.write(".harness/policy.toml", "[budget]\nmax_tokens = 20\n");
    let ran = run(&ws, &five_chunks(), Launch::default()).await;
    assert_eq!(ran.report()["llm_calls"], 2);

    let ws = Workspace::new();
    ws.write(".harness/policy.toml", "[budget]\nmax_tokens = 20\n");
    let ran = run(&ws, &five_chunks(), Launch::max_tokens(1000)).await;
    assert_eq!(ran.report()["finished"], true);
}

#[tokio::test]
async fn without_a_budget_the_run_completes() {
    let ws = Workspace::new();
    let ran = run(&ws, &five_chunks(), Launch::default()).await;
    let report = ran.report();
    assert_eq!(report["finished"], true);
    assert!(report.get("stopped").is_none(), "{report}");
}

#[tokio::test]
async fn a_wall_budget_of_zero_blocks_the_first_call() {
    let ws = Workspace::new();
    let ran = run(&ws, &five_chunks(), Launch::max_wall(0.0)).await;
    let report = ran.report();
    assert_eq!(report["llm_calls"], 0);
    assert!(
        report["stopped"].as_str().unwrap().contains("deadline"),
        "{report}"
    );
}

#[tokio::test]
async fn a_subagent_shares_its_parents_token_budget() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![
        luau("local r = agent(\"sub\")\nprint(r.answer)"),
        luau("print(1)"),
        luau("print(2)"),
        luau("print(3)"),
        text("done"),
    ]);
    let ran = run(&ws, &model, Launch::max_tokens(40)).await;
    let report = ran.report();
    assert_eq!(report["llm_calls"], 3);
    assert!(
        report["stopped"]
            .as_str()
            .unwrap()
            .contains("budget_exceeded"),
        "{report}"
    );
}

#[tokio::test]
async fn cached_reads_and_writes_count_toward_the_token_budget() {
    let ws = Workspace::new();
    let model = Scripted::replies(vec![luau("print(1)").usage(100, 90, 9, 1)]);
    let ran = run(&ws, &model, Launch::max_tokens(100)).await;
    let report = ran.report();
    assert_eq!(model.bodies().len(), 1);
    assert_eq!(report["budget"]["tokens"], 101);
    assert_eq!(report["finished"], false);
    assert!(
        report["stopped"]
            .as_str()
            .unwrap()
            .contains("token admission threshold")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_effect_cut_off_by_the_run_deadline_is_always_recorded_as_uncertain() {
    // the deadline falls while a shell command runs: its intent never goes without an outcome or an uncertain record
    let runs = (0..6).map(|_| async {
        let ws = Workspace::new();
        let model = Scripted::replies(vec![
            luau("local r = sh(\"sleep 5; echo hi\")\nreturn r.stdout"),
            text("done"),
        ]);
        let ran = run(&ws, &model, Launch::max_wall(1.5)).await;
        let listed = ran.report()["uncertain_operations"]
            .as_array()
            .unwrap()
            .len();
        (listed <= 1, ran.records("uncertain").len())
    });
    for outcome in futures::future::join_all(runs).await {
        assert_eq!(outcome, (true, 1));
    }
}
