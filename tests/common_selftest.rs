//! Self-tests for the shared test harness.
//!
//! The helpers in `tests/common` decide whether every other integration test
//! sees a real failure or a startup race. A helper that silently always
//! succeeded — or one that gave up before the bridges could come up — would
//! turn the whole suite green or red for the wrong reason, so the harness gets
//! tested like anything else.

mod common;

use std::time::Duration;

/// `retry_client` must give up and report the last error, not loop forever.
///
/// It is the gate every racy client goes through; if it could not fail, a
/// genuinely broken bridge would hang the suite instead of failing it.
#[tokio::test]
async fn retry_client_gives_up_and_reports_the_last_error() {
    let started = tokio::time::Instant::now();
    let result: anyhow::Result<()> = common::retry_client(
        || async { Err::<(), _>(std::io::Error::other("backend is never coming")) },
        Duration::from_millis(600),
        "an attempt that can never succeed",
    )
    .await;
    let elapsed = started.elapsed();

    let err = result.expect_err("an attempt that always fails must not report success");
    let err = err.to_string();
    assert!(
        err.contains("backend is never coming"),
        "the last underlying error must survive into the report, got: {err}"
    );
    assert!(
        err.contains("an attempt that can never succeed"),
        "the description must name what was being retried, got: {err}"
    );
    assert!(
        elapsed >= Duration::from_millis(600),
        "gave up after {elapsed:?}, before its own deadline"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "took {elapsed:?} to honour a 600ms deadline"
    );
}

/// The happy path must not pay for the retry machinery: an attempt that
/// succeeds immediately returns immediately, and returns its value.
#[tokio::test]
async fn retry_client_returns_the_first_success_without_delay() {
    let mut calls = 0;
    let started = tokio::time::Instant::now();
    let value = common::retry_client(
        || {
            calls += 1;
            async move { Ok::<_, std::io::Error>(42) }
        },
        Duration::from_secs(30),
        "an attempt that succeeds at once",
    )
    .await
    .expect("a succeeding attempt must be reported as success");

    assert_eq!(value, 42, "the attempt's value must be handed back");
    assert_eq!(calls, 1, "a success must not be retried");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a success must return without waiting out a backoff"
    );
}

/// A later attempt succeeding is the whole point: the helper exists because the
/// export side announces itself after the listener is already accepting.
#[tokio::test]
async fn retry_client_succeeds_once_the_attempt_starts_working() {
    let mut attempts = 0;
    let value = common::retry_client(
        || {
            attempts += 1;
            let attempt = attempts;
            async move {
                if attempt < 3 {
                    Err(std::io::Error::other("not ready yet"))
                } else {
                    Ok("routed")
                }
            }
        },
        common::BACKEND_READY_TIMEOUT,
        "an attempt that starts working on the third try",
    )
    .await
    .expect("must succeed once the attempt starts working");

    assert_eq!(value, "routed");
    assert_eq!(
        attempts, 3,
        "should stop at the first success, not overshoot"
    );
}
