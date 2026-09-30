use super::*;

#[tokio::test]
async fn terminal_watch_attaches_before_snapshot_and_preserves_racing_completion() {
    for scenario in [
        "events_attach",
        "events_attach_cancel",
        "events_stale_read",
        "early_complete",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let relay = Relay::start(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/support/fake_codex.py"),
            Host::open(directory.path()).unwrap(),
        )
        .await
        .unwrap();
        let facts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = facts.clone();
        relay
            .set_terminal_observer(Arc::new(move |fact| {
                observed.lock().unwrap().push(fact);
                Ok(())
            }))
            .unwrap();
        let worker = relay
            .work_start(
                scenario.into(),
                Some(directory.path().display().to_string()),
                None,
                None,
                None,
                Some("fixture-model-1".into()),
                None,
                Some(SandboxPolicy::DangerFullAccess),
            )
            .await
            .unwrap();
        let thread = worker["threadId"].as_str().unwrap();
        let turn = worker["turnId"].as_str().unwrap();
        // Drain turn/started before simulating observation of a previously
        // detached worker. The peer completes it during the next resume.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if relay
                    .journal
                    .read_after(0, thread, Some(turn))
                    .await
                    .unwrap()
                    .events
                    .iter()
                    .any(|event| event.method == "turn/started")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if scenario.starts_with("events_attach") {
            relay
                .app_server
                .request(ThreadUnsubscribe {
                    thread_id: thread.into(),
                })
                .await
                .unwrap();
            *relay.thread_subscriptions.lock().await = ThreadSubscriptions::default();
            *relay.live_turns.lock().await = LiveTurns::default();
        }
        if scenario == "events_attach_cancel" {
            let watching = relay.clone();
            let thread = thread.to_owned();
            let turn = turn.to_owned();
            let task = tokio::spawn(async move { watching.watch_terminal(&thread, &turn).await });
            tokio::time::timeout(Duration::from_secs(5), async {
                while !directory.path().join("events-resume-entered").exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        relay.watch_terminal(thread, turn).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if relay
                    .live_turn(thread, turn)
                    .await
                    .is_some_and(|t| t.status.is_terminal())
                    && !relay
                        .thread_subscriptions
                        .lock()
                        .await
                        .is_subscribed(thread)
                    && !facts.lock().unwrap().is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("terminal observation lost for {scenario}"));
        assert!(facts.lock().unwrap().iter().all(|fact| {
            fact.thread_id == thread && fact.turn_id == turn && fact.status == "completed"
        }));
    }
}
