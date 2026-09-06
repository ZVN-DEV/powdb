//! Gate acquisition and the transaction-lifetime deadline.

use super::*;

// ---- Explicit-transaction gate wait timeout (P-4) ----

/// `begin` takes more than half the pool: enough that no second explicit
/// transaction and no autocommit writer can start, and no more, so readers
/// keep running beside a transaction that has written nothing.
#[tokio::test]
async fn begin_permit_acquires_when_gate_is_free() {
    let gate = new_tx_gate();
    let metrics = Arc::new(Metrics::new());
    let mut permit = Some(
        acquire_begin_permit(&gate, Duration::from_secs(5), &metrics)
            .await
            .expect("should acquire a free gate"),
    );
    let held = gate.begin_permits() as usize;
    assert!(
        held * 2 > DEFAULT_TX_GATE_READER_PERMITS as usize,
        "a begin must hold more than half the pool or two could overlap"
    );
    assert_eq!(
        gate.available_permits(),
        DEFAULT_TX_GATE_READER_PERMITS as usize - held,
        "the rest of the pool must stay available to readers"
    );

    // The transaction's first write takes the remainder.
    upgrade_to_exclusive(&gate, &mut permit, Duration::from_secs(5), &metrics)
        .await
        .expect("upgrade on an uncontended gate");
    assert_eq!(
        gate.available_permits(),
        0,
        "a transaction that has written must hold the whole gate"
    );

    drop(permit);
    assert_eq!(
        gate.available_permits(),
        DEFAULT_TX_GATE_READER_PERMITS as usize,
        "permit pool must release on drop"
    );
}

/// The upgrade at the first write is what excludes readers, and it waits for
/// the readers already running instead of cutting them off.
#[tokio::test]
async fn the_first_write_waits_for_readers_then_excludes_them() {
    let gate = new_tx_gate();
    let metrics = Arc::new(Metrics::new());
    let mut permit = Some(
        acquire_begin_permit(&gate, Duration::from_secs(5), &metrics)
            .await
            .expect("begin admission"),
    );
    let reader = acquire_autocommit_permit(
        &gate,
        AdmissionMode::Reader,
        Duration::from_secs(1),
        &metrics,
    )
    .await
    .expect("a reader must be admitted beside a begin-only transaction");

    let upgrade_gate = gate.clone();
    let upgrade_metrics = metrics.clone();
    let mut upgrade = tokio::spawn(async move {
        let outcome = upgrade_to_exclusive(
            &upgrade_gate,
            &mut permit,
            Duration::from_secs(5),
            &upgrade_metrics,
        )
        .await;
        (outcome, permit)
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut upgrade)
            .await
            .is_err(),
        "the first write must wait for the reader that is already running"
    );

    drop(reader);
    let (outcome, permit) = tokio::time::timeout(Duration::from_secs(5), upgrade)
        .await
        .expect("the upgrade must complete once the reader releases")
        .expect("upgrade task");
    outcome.expect("upgrade must succeed once no reader is running");
    assert_eq!(
        gate.available_permits(),
        0,
        "a transaction that has written must exclude every reader"
    );
    drop(permit);
    assert_eq!(
        gate.available_permits(),
        DEFAULT_TX_GATE_READER_PERMITS as usize,
        "the whole pool must come back when the transaction ends"
    );
}

#[tokio::test]
async fn begin_permit_times_out_with_clear_error_and_truthful_metric() {
    let gate = new_tx_gate();
    let metrics = Arc::new(Metrics::new());
    // Hold the full writer admission so the next acquire must time out.
    let _held = gate
        .clone()
        .acquire_many_owned(DEFAULT_TX_GATE_READER_PERMITS)
        .await
        .unwrap();
    let err = acquire_begin_permit(&gate, Duration::from_millis(25), &metrics)
        .await
        .expect_err("must time out while the gate is held");
    match err {
        Message::ErrorWithClass { message, class } => {
            assert_eq!(class, ErrorClass::Timeout);
            assert!(
                message.contains("transaction gate timeout after 25ms"),
                "unexpected message: {message}"
            );
            assert!(
                message.contains("waiting for concurrent transaction"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
    let rendered = metrics.render();
    assert!(rendered.contains("powdb_tx_gate_timeouts_total 1"));
    // A timed-out begin is a failed statement from the client's view.
    assert!(rendered.contains("powdb_queries_total{result=\"error\"} 1"));
}

/// The transaction-lifetime deadline is derived from the permit, so it
/// cannot be armed on some install sites and forgotten on others, and a
/// later frame on the SAME transaction cannot re-arm it (which is exactly
/// how a `PING` loop defeated the idle deadline).
#[tokio::test]
async fn transaction_deadline_tracks_the_permit_and_never_re_arms_mid_transaction() {
    let gate = new_tx_gate_with_permits(1);
    let metrics = Arc::new(Metrics::new());
    let max = Some(Duration::from_secs(60));
    let mut deadline: Option<Instant> = None;
    let mut permit: Option<OwnedSemaphorePermit> = None;

    sync_tx_deadline(&permit, &mut deadline, max);
    assert!(deadline.is_none(), "no transaction, no deadline");

    permit = Some(
        acquire_begin_permit(&gate, Duration::from_secs(1), &metrics)
            .await
            .expect("begin permit"),
    );
    sync_tx_deadline(&permit, &mut deadline, max);
    let armed = deadline.expect("an open transaction arms the deadline");

    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(2)).await;
        sync_tx_deadline(&permit, &mut deadline, max);
    }
    assert_eq!(
        deadline,
        Some(armed),
        "frames inside a transaction must not push its deadline out"
    );

    permit = None;
    sync_tx_deadline(&permit, &mut deadline, max);
    assert!(deadline.is_none(), "releasing the gate clears the deadline");

    permit = Some(
        acquire_begin_permit(&gate, Duration::from_secs(1), &metrics)
            .await
            .expect("second begin permit"),
    );
    sync_tx_deadline(&permit, &mut deadline, max);
    assert!(
        deadline.is_some_and(|next| next >= armed),
        "a new transaction gets a fresh deadline, not the previous one"
    );

    // The documented opt-out.
    let mut unbounded: Option<Instant> = None;
    sync_tx_deadline(&permit, &mut unbounded, None);
    assert!(unbounded.is_none());
    drop(permit);
}
