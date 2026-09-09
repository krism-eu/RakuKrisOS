//! Live-HTTP verification of the *same-repo* mirror fallback in
//! `download_all_ex`: when a `mirrorlist=`/`metalink=` repo's chosen mirror
//! 404s on an individual package file (the real Fedora bug this reproduces —
//! `ix-denver.mm.fcix.net` 404ing on `Box2D-2.4.2-7.fc44.x86_64.rpm` while
//! its own `primary.xml` still advertised it), the download should retry
//! the same `href` against the rest of that repo's mirrorlist instead of
//! failing the whole transaction. Also verifies the fix in
//! `rum-repo/src/net.rs`: a definitive 404 must fail fast rather than
//! burning through every configured retry attempt's exponential backoff
//! before the mirror-swap loop ever gets a turn.

use rum_core::{Nevra, Package};
use rum_repo::RepoConfig;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Minimal HTTP server: replies with `status_line` + `body` to every
/// request it accepts. `count` is a lower bound on how many requests it
/// must be ready to serve, not an exact quota — librepo's own mirror-speed
/// ranking (`lr_fastestmirror`, wired in since this test was first written)
/// makes its own real TCP connect probes against each mirror before
/// `download_all_ex` ever gets to it, on top of whatever this crate's own
/// HTTP client does, so the exact connection count is no longer something
/// a test can pin down — it accepts indefinitely instead, and tolerates a
/// probe connection that connects and disconnects without ever completing
/// a request (`write_all`'s error is ignored rather than unwrapped).
fn spawn_server(count: usize, status_line: &'static str, body: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let response = format!("{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            if stream.write_all(response.as_bytes()).is_err() {
                continue;
            }
            let _ = stream.write_all(body);
        }
    });
    let _ = count;
    format!("http://{addr}")
}

/// Serves a fixed body to every request it accepts (used for the
/// `mirrorlist=` text endpoint, whose exact request count isn't worth
/// pinning down — `resolve_mirrors` is only called once per download here,
/// but being generous avoids a flaky hang if that ever changes).
fn spawn_static_server(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn pkg(name: &str, location: String) -> Package {
    Package {
        nevra: Nevra { name: name.to_string(), epoch: 0, version: "2.4.2".to_string(), release: "7.fc44".to_string(), arch: "x86_64".to_string() },
        summary: String::new(),
        provides: vec![],
        requires: vec![],
        conflicts: vec![],
        obsoletes: vec![],
        recommends: vec![],
        suggests: vec![],
        enhances: vec![],
        supplements: vec![],
        location,
        repo_id: "fedora".to_string(),
        repo_priority: 99,
        repo_cost: 1000,
        install_size: 0,
        download_size: 0,
            vendor: String::new(),
            checksum_type: String::new(),
            checksum: String::new(),
    }
}

fn repo_cfg(id: &str, mirrorlist_url: String) -> RepoConfig {
    RepoConfig {
        id: id.to_string(),
        name: id.to_string(),
        base_url: None,
        mirrorlist: Some(mirrorlist_url),
        metalink: None,
        gpgcheck: false,
        gpgkeys: vec![],
        metadata_expire: rum_repo::DEFAULT_METADATA_EXPIRE,
        enabled: true,
        priority: 99,
        cost: 1000,
        exclude: vec![],
        includepkgs: vec![],
        skip_if_unavailable: None,
        proxy: None,
        proxy_username: None,
        proxy_password: None,
        username: None,
        password: None,
        sslcacert: None,
        sslclientcert: None,
        sslclientkey: None,
    }
}

/// Reproduces the real `ix-denver.mm.fcix.net` Box2D 404: the repo's
/// mirrorlist has two mirrors, the package's `location` was resolved
/// against the first (now 404ing on this one file), and the second mirror
/// actually has it. The whole download must still succeed by falling
/// through to the second mirror.
#[tokio::test]
async fn falls_back_to_the_next_mirror_in_the_same_repos_mirrorlist_on_404() {
    // Configure a high retry count, same as a user's `retries=10` in
    // rum.conf — this is what makes the fast-fail-on-404 fix in
    // rum-repo/src/net.rs load-bearing: without it, this single dead mirror
    // would burn ~10 attempts' worth of exponential backoff before the
    // mirror-swap loop below ever got a turn.
    rum_repo::set_max_attempts(10);

    // `download_all_ex` also ranks mirrors fastest-first via librepo's own
    // `lr_fastestmirror` (`resolve_ranked_mirrors` -> `rum_repo::
    // fastest_mirror_sort`, real TCP connect-time probes, same mechanism
    // dnf5 uses) and re-probes for a currently-best mirror via
    // `resolve_base_url` (a `HEAD` per mirror, in ranked order, stopping at
    // the first success) before its own fetch — and the fetch itself goes
    // through `ripget`, which issues its own initial `Range: bytes=0-0`
    // metadata probe before the real data connection. Exactly how many
    // connections each mirror gets from all of that is an implementation
    // detail this test no longer pins down (see `spawn_server`); what
    // matters is that the dead mirror's 404 doesn't stop the alive mirror
    // from being found and used.
    let dead_mirror = spawn_server(2, "HTTP/1.1 404 Not Found", b"not found");
    let alive_mirror = spawn_server(4, "HTTP/1.1 200 OK", b"fake Box2D rpm bytes");
    let mirrorlist_body: &'static str = Box::leak(format!("{dead_mirror}\n{alive_mirror}\n").into_boxed_str());
    let mirrorlist_url = spawn_static_server(mirrorlist_body);

    let cfg = repo_cfg("fedora", mirrorlist_url);
    let href = "Everything/x86_64/os/Packages/b/Box2D-2.4.2-7.fc44.x86_64.rpm";
    let package = pkg("Box2D", format!("{dead_mirror}/{href}"));

    let tmp = tempdir();
    let client = reqwest::Client::new();

    let start = Instant::now();
    let result = rum_transaction::download_all_ex(&client, &[package], &tmp, 1, false, std::slice::from_ref(&cfg), &[]).await.expect("should fall back to the alive mirror");
    let elapsed = start.elapsed();

    assert_eq!(result.len(), 1);
    assert!(result[0].rpm_path.exists());
    assert_eq!(std::fs::read(&result[0].rpm_path).unwrap(), b"fake Box2D rpm bytes");

    // The whole thing (one dead-mirror 404 + one successful retry) must
    // complete near-instantly. Before the fast-fail fix, a definitive 404
    // was retried `max_attempts` times with `200ms * 2^attempt` backoff —
    // at `retries=10` that alone is well over a minute before the
    // mirror-swap loop even got to try the second mirror.
    assert!(elapsed < Duration::from_secs(5), "mirror fallback took {elapsed:?}, the dead mirror's 404 should fail fast instead of exhausting its retry budget");
}

/// Reproduces the real `gtk-vnc2` bug: `location` was built against a
/// mirror (`baseurl.txt`, from a metadata fetch that may be hours old) that
/// simply doesn't appear in a *fresh* `resolve_mirrors` call — Fedora's
/// metalink endpoint returns a geo-sorted/randomized subset per request, so
/// two calls needn't agree on which mirrors show up at all. Before this
/// fix, matching `location`'s mirror by prefix-searching the fresh list
/// silently found nothing, so no fallback URLs were ever added and the
/// download just failed on the stale mirror's 404 with no retry.
#[tokio::test]
async fn falls_back_even_when_the_used_mirror_is_absent_from_a_fresh_resolve() {
    rum_repo::set_max_attempts(10);

    // `stale_mirror` is only ever the *original* location's mirror; since
    // the fresh metalink resolve never mentions it, it's tried strictly
    // after the (correctly preferred) alive mirror and is never actually
    // reached once the alive mirror succeeds. `alive_mirror` here is the
    // *only* mirror in the fresh resolve, so `resolve_ranked_mirrors`/
    // `resolve_base_url` both short-circuit without any network probe
    // (`mirrors.len() <= 1`) — its connections are purely `ripget`'s own
    // `Range: bytes=0-0` probe plus the real fetch.
    let stale_mirror = spawn_server(1, "HTTP/1.1 404 Not Found", b"not found");
    let alive_mirror = spawn_server(2, "HTTP/1.1 200 OK", b"fake gtk-vnc2 rpm bytes");
    // The *fresh* metalink response only ever mentions `alive_mirror` — it
    // never lists `stale_mirror`, simulating a geo/randomized re-resolve
    // that disagrees with whatever mirror `baseurl.txt` was cached from.
    let metalink_body: &'static str = Box::leak(
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<metalink version="3.0" xmlns="http://www.metalinker.org/">
<files><file name="repomd.xml">
<resources>
<url protocol="http" type="http" location="US" preference="100">{alive_mirror}/repodata/repomd.xml</url>
</resources>
</file></files>
</metalink>"#
        )
        .into_boxed_str(),
    );
    let metalink_url = spawn_static_server(metalink_body);

    let cfg = RepoConfig {
        id: "fedora".to_string(),
        name: "fedora".to_string(),
        base_url: None,
        mirrorlist: None,
        metalink: Some(metalink_url),
        gpgcheck: false,
        gpgkeys: vec![],
        metadata_expire: rum_repo::DEFAULT_METADATA_EXPIRE,
        enabled: true,
        priority: 99,
        cost: 1000,
        exclude: vec![],
        includepkgs: vec![],
        skip_if_unavailable: None,
        proxy: None,
        proxy_username: None,
        proxy_password: None,
        username: None,
        password: None,
        sslcacert: None,
        sslclientcert: None,
        sslclientkey: None,
    };

    let href = "Everything/x86_64/os/Packages/g/gtk-vnc2-1.5.0-4.fc44.x86_64.rpm";
    let package = pkg("gtk-vnc2", format!("{stale_mirror}/{href}"));

    let tmp = tempdir();
    // Simulate a metadata cache written against `stale_mirror` at some
    // earlier point in time — exactly what `load_repo_ex` would have left
    // behind on disk before this mirror went dead for this one file.
    let cache_root = tmp.join("repos");
    let cache_dir = rum_repo::repo_cache_dir(&cache_root, &cfg);
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(cache_dir.join("baseurl.txt"), &stale_mirror).unwrap();

    let client = reqwest::Client::new();
    let result = rum_transaction::download_all_ex(&client, &[package], &tmp, 1, false, std::slice::from_ref(&cfg), &[]).await.expect("should fall back despite the mirror mismatch");

    assert_eq!(result.len(), 1);
    assert_eq!(std::fs::read(&result[0].rpm_path).unwrap(), b"fake gtk-vnc2 rpm bytes");
}

fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rum-mirror-fallback-test-{}", std::process::id())).join(uniqueish());
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn uniqueish() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(), COUNTER.fetch_add(1, Ordering::Relaxed))
}
