//! Live verification that a connection trickling well below `minrate=`
//! bytes/sec is treated as stalled and aborted, not waited on forever —
//! the gap that let a mirror crawling at a few bytes/sec ("12 B/s eta 23w",
//! live-observed) hang indefinitely under the old zero-progress-only stall
//! check. Uses a real local TCP server dribbling a handful of bytes/sec so
//! this exercises the actual `tokio::select!`/interval-based watchdogs in
//! `rum_repo::net`, not just the pure `parse_bytes` helper.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

/// Accepts one connection, sends a `Content-Length` header for `body_len`
/// bytes, then trickles `per_tick` bytes every second forever (until the
/// client gives up and closes) — never actually completing the transfer,
/// so any success here would mean the stall watchdog never fired.
fn spawn_trickle_server(body_len: usize, per_tick: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else { return };
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n");
        if stream.write_all(header.as_bytes()).is_err() {
            return;
        }
        loop {
            if stream.write_all(&vec![b'x'; per_tick]).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn get_bytes_with_retry_aborts_on_sub_minrate_throughput() {
    // 1 attempt (fail fast rather than retrying the same dead trickle 4x),
    // a 2s stall window, and a 1000 B/s floor — the server sends 5 B/s,
    // well under that floor, so the very first stall check should trip.
    rum_repo::set_max_attempts(1);
    rum_repo::set_stall_timeout(2);
    rum_repo::set_minrate(1000);

    let url = spawn_trickle_server(2_000_000, 5);
    let client = reqwest::Client::new();

    let start = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(10), rum_repo::get_bytes_with_retry(&client, &url)).await;
    let elapsed = start.elapsed();

    // Must resolve (not hit our outer 10s test timeout) and must be an
    // error — a trickle this slow is never going to finish 2 MiB.
    let result = result.expect("stall watchdog never fired — hung past the outer test timeout");
    assert!(result.is_err(), "expected a stall error, got a successful download");
    assert!(elapsed < Duration::from_secs(6), "took {elapsed:?} to detect the stall, expected ~2s");
}

#[tokio::test]
async fn download_to_file_with_retry_ex_aborts_on_sub_minrate_throughput() {
    rum_repo::set_max_attempts(1);
    rum_repo::set_stall_timeout(2);
    rum_repo::set_minrate(1000);

    let url = spawn_trickle_server(2_000_000, 5);
    let client = reqwest::Client::new();
    let dir = std::env::temp_dir().join(format!("rum-minrate-test-{}", std::process::id()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let dest = dir.join("pkg.rpm");

    let start = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(15), rum_repo::download_to_file_with_retry_ex(&client, &url, &dest, None, None)).await;
    let elapsed = start.elapsed();

    let result = result.expect("stall watchdog never fired — hung past the outer test timeout");
    assert!(result.is_err(), "expected a stall error, got a successful download");
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?} to detect the stall, expected a few seconds");
}
