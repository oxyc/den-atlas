//! Shutdown behaviour, against the real binary.
//!
//! These cannot be unit tests: the paths they cover end in `std::process::exit`, and a signal is
//! process-wide, so exercising them in-process would take down the test runner. Deleting the whole
//! second-signal feature — the "press ^C again" escape hatch, which had already been silently broken
//! once — passed every one of the 114 unit tests.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Start den-atlas on an ephemeral port and wait until it is answering.
fn start() -> (Child, u16) {
    let dir = std::env::temp_dir().join(format!("den-atlas-shutdown-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // No dataset: `main` fails soft to catalog-only, which is enough to serve /health.
    let mut child = Command::new(env!("CARGO_BIN_EXE_den-atlas"))
        .env("ATLAS_DATA_DIR", &dir)
        .env("PORT", "0")
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("the binary must start");

    // The listening line carries the port it actually bound. It is not necessarily the first line —
    // a missing dataset is reported before it — so read until it appears.
    let stderr = child.stderr.take().unwrap();
    let mut reader = BufReader::new(stderr);
    let mut port = None;
    for _ in 0..10 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Some(rest) = line.split(" on :").nth(1) {
            port = rest.split_whitespace().next().and_then(|p| p.parse().ok());
            break;
        }
    }
    let port: u16 = port.expect("the binary never reported a listening port");
    // Keep draining stderr so the pipe cannot fill and block the child.
    std::thread::spawn(move || {
        let mut sink = String::new();
        while reader.read_line(&mut sink).unwrap_or(0) > 0 {
            sink.clear();
        }
    });
    (child, port)
}

fn signal(child: &Child, sig: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(child.id() as i32, sig) };
}

const SIGTERM: i32 = 15;
const SIGINT: i32 = 2;

/// A client holding half a request head keeps the drain busy for its full grace — unless a second
/// signal arrives, which must end it immediately. Without that, tokio has already dropped both
/// `Signal` handles and does NOT restore the default disposition, so every later SIGTERM and ^C is
/// caught and discarded and only SIGKILL works.
#[test]
fn a_second_signal_ends_the_drain_at_once() {
    for sig in [SIGTERM, SIGINT] {
        let (mut child, port) = start();
        let mut stuck = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stuck.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n").unwrap(); // no blank line
        stuck.flush().unwrap();
        std::thread::sleep(Duration::from_millis(200));

        signal(&child, SIGTERM);
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        signal(&child, sig);

        let status = child.wait().expect("wait");
        let took = started.elapsed();
        assert!(
            took < Duration::from_secs(3),
            "a second signal ({sig}) did not end the drain: took {took:?} of the 8s grace"
        );
        assert_eq!(status.code(), Some(0), "asking twice is deliberate, not a failure");
    }
}

/// ...and ONE signal still drains to the deadline rather than exiting at once — otherwise the test
/// above would pass with no drain at all.
#[test]
fn one_signal_still_drains() {
    let (mut child, port) = start();
    let mut stuck = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stuck.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n").unwrap();
    stuck.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let started = Instant::now();
    signal(&child, SIGTERM);
    let status = child.wait().expect("wait");
    let took = started.elapsed();

    assert!(took > Duration::from_secs(4), "it did not drain at all: {took:?}");
    assert!(took < Duration::from_secs(12), "the drain is unbounded again: {took:?}");
    // A deadline reached is a designed outcome, not a crash: exiting non-zero put the unit in
    // `failed`, and any client can cause it.
    assert_eq!(status.code(), Some(0), "a routine drain timeout reported a crash");
}

/// An idle server exits immediately and cleanly — the grace costs nothing when nothing is in flight.
#[test]
fn an_idle_server_exits_at_once() {
    let (mut child, _port) = start();
    let started = Instant::now();
    signal(&child, SIGTERM);
    let status = child.wait().expect("wait");
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "idle exit took {took:?}");
    assert_eq!(status.code(), Some(0));
}
