//! What an unauthenticated or misbehaving peer can make the server do:
//! bounded lines, no request payloads, an absolute auth deadline and a
//! connection cap.

use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{rig, Client};
use crate::server::{MAX_CONNECTIONS, PRE_AUTH_LINE};

/// The error code of the next frame, then whether the server closed.
fn refused_and_closed(c: &mut Client) -> (String, bool) {
    let code = c
        .read()
        .map(|f| f.msg["error"]["code"].as_str().unwrap_or("").to_owned())
        .unwrap_or_default();
    (code, c.read().is_none())
}

#[test]
fn an_oversized_line_before_auth_is_refused() {
    let rig = rig("harden-line", false);
    let mut c = Client::connect(&rig);
    let pad = "x".repeat(PRE_AUTH_LINE + 100);
    c.send(json!({"id": 1, "method": "auth", "params": {"token": rig.token, "pad": pad}}));
    assert_eq!(refused_and_closed(&mut c), ("bad_request".to_owned(), true));
}

#[test]
fn a_request_payload_is_refused() {
    let rig = rig("harden-binary", false);
    // Before auth: nothing is buffered, the peer is dropped.
    let mut c = Client::connect(&rig);
    c.send(
        json!({"id": 1, "method": "auth", "params": {"token": rig.token}, "binary": 67_108_864}),
    );
    assert_eq!(refused_and_closed(&mut c), ("bad_request".to_owned(), true));
    // After auth too: the payload would desynchronise the stream.
    let mut c = Client::authed(&rig);
    c.send(json!({"id": 2, "method": "state", "binary": 4}));
    assert_eq!(refused_and_closed(&mut c), ("bad_request".to_owned(), true));
}

#[test]
fn a_trickling_peer_is_dropped_at_the_deadline() {
    let rig = rig("harden-trickle", false);
    rig.shared.auth_timeout_ms.store(300, Ordering::Relaxed);
    let mut stream = TcpStream::connect(rig.shared.local_addr).unwrap();
    let started = Instant::now();
    let mut dropped = false;
    // One byte every 50 ms never trips a per-read timeout.
    for byte in
        b"{\"id\":1,\"method\":\"auth\",\"params\":{\"token\":\"0000000000000000\"}}".repeat(4)
    {
        if stream.write_all(&[byte]).is_err() {
            dropped = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
        if started.elapsed() > Duration::from_secs(3) {
            break;
        }
    }
    let mut c = Client::from_stream(stream);
    if !dropped {
        dropped = refused_and_closed(&mut c).1;
    }
    assert!(dropped, "still connected after {:?}", started.elapsed());
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn connections_are_capped_and_slots_released() {
    let rig = rig("harden-cap", false);
    let held: Vec<Client> = (0..MAX_CONNECTIONS).map(|_| Client::authed(&rig)).collect();
    let mut extra = Client::connect(&rig);
    extra.send(json!({"id": 1, "method": "auth", "params": {"token": rig.token}}));
    assert!(extra.read().is_none(), "one more than the cap is closed");
    drop(held);
    // The slots come back once the client threads notice the close.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut c = Client::connect(&rig);
        let parts = {
            c.send(json!({"id": 1, "method": "auth", "params": {"token": rig.token}}));
            c.read()
        };
        if parts.is_some_and(|f| f.msg["ok"] == Value::Bool(true)) {
            break;
        }
        assert!(Instant::now() < deadline, "slots not released");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn log_lines_dropped_on_a_full_outbox_are_reported_as_missed() {
    let _ring = super::LOG_RING.lock();
    let rig = rig("harden-dropped", false);
    let mut c = Client::authed(&rig);
    c.ok(
        "log.subscribe",
        json!({"level": "debug", "targets": ["sl"]}),
    );
    // Fill the socket buffers and the outbox while the client reads
    // nothing; batches stay well inside the ring so nothing falls out of it.
    let pad = "p".repeat(4000);
    let (batches, per) = (40, 200);
    for _ in 0..batches {
        for _ in 0..per {
            log::debug!(target: "sl", "flood {pad}");
        }
        thread::sleep(Duration::from_millis(60));
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ender = {
        let stop = stop.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                log::debug!(target: "sl", "flood end");
                thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let (mut delivered, mut missed) = (0u64, 0u64);
    loop {
        let e = c.event("log");
        missed += e["missed"].as_u64().unwrap_or(0);
        if e["message"] == "flood end" {
            break;
        }
        delivered += 1;
    }
    stop.store(true, Ordering::Release);
    ender.join().unwrap();
    assert!(
        missed > 0,
        "the outbox never overflowed ({delivered} delivered)"
    );
    assert!(
        delivered + missed >= batches * per,
        "{delivered} delivered + {missed} missed < {}",
        batches * per
    );
}
