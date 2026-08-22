//! Lock-contention integration test for `hbird update-cluster` (#321 round-2 lens L5#M1).
//!
//! Verifies that a second `hbird update-cluster` invocation against the
//! same XDG_RUNTIME_DIR (where the per-user flock lives) fails fast with
//! the documented "another update-cluster run is in progress" wording
//! while a first invocation holds the lock.
//!
//! This lives in tests/ rather than as an inline unit test because the
//! test needs an isolated XDG_RUNTIME_DIR. `std::env::set_var` on Rust
//! 2024 edition requires the `unsafe` keyword (env mutation is unsound
//! in a multithreaded process), and the workspace lint
//! `unsafe_code = "forbid"` rules that out in `src/`. Spawning a
//! subprocess with the env set at spawn time sidesteps the issue.
//!
//! The first invocation is parked on a long-running flag set (we use a
//! synthetic config that wedges live-mode IP resolution, which is
//! enough to keep `acquire_lock` held while the binary unwinds).
//! Actually — `acquire_lock` is called BEFORE `from_args`'s live-IP
//! resolution returns the live-mode error. Read the source: the order
//! in `run()` is parse → Plan::from_args → acquire_lock. So a live-mode
//! IP error short-circuits BEFORE the lock is taken. We need a path
//! that holds the lock for measurable wall time.
//!
//! Strategy: invoke `--dry-run` against a CONFIGURED cluster, which
//! makes `acquire_lock` skip (dry-run no-ops the lock). That's not
//! contention. Instead we drive the test through the in-process unit
//! that contention would surface — running two threads in the same
//! process is moot under env-race, AND `acquire_lock` only takes the
//! lock when `dry_run=false`. So the integration test below relies on:
//!
//! - Two `hbird` subprocesses run with the same XDG_RUNTIME_DIR.
//! - Both have `dry_run=false`, so both call `acquire_lock`.
//! - To prevent the first one from blowing past the lock quickly
//!   (because Plan::from_args will reject live-mode without static
//!   IPs), we need the first invocation to *hold* the lock for at
//!   least a few hundred ms. Easiest: provide static IPs in the config
//!   so Plan::from_args succeeds; then the first SSH/kubectl call
//!   bails out with live-mode-not-implemented — but only AFTER the
//!   lock is held.
//!
//! That's how we deterministically pin contention.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// Kill a spawned holder and every process in its group, then reap it.
/// `Child::kill` alone leaves the shell that holds the inherited lock fd.
fn kill_group(child: &mut std::process::Child) {
    let pgid = child.id() as i32;
    // TERM the whole group, then KILL it: a holder that ignores or defers
    // TERM must not survive and keep the lock held.
    let _ = Command::new("kill")
        .args(["-TERM", &format!("-{pgid}")])
        .status();
    let _ = Command::new("kill")
        .args(["-KILL", &format!("-{pgid}")])
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

fn hbird_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hbird"))
}

/// Write a static-IP cluster.local.conf so Plan::from_args succeeds
/// past the live-mode IP-resolution bail. The first live-mode SSH
/// call will still surface a "live-mode not implemented" diagnostic,
/// but the lock will be HELD by that point — which is what the test
/// needs to assert contention.
fn write_static_ip_config(path: &std::path::Path) {
    std::fs::write(
        path,
        "CP_NAME=hbird-cp1\n\
         SSH_PUBKEY_FILE=/k\n\
         CP_IP=192.0.2.10\n\
         WORKER_NAMES=(hbird-w1 hbird-w2)\n\
         WORKER_IPS=(192.0.2.11 192.0.2.12)\n",
    )
    .expect("write static-ip fixture config");
}

/// Per-test tempdir that's removed on drop. Same pattern as the dry-run
/// fixtures harness.
struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tempdir_for_test(label: &str) -> TempDir {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("hbird-uc-lock-{label}-{pid}-{n}"));
    std::fs::create_dir_all(&dir).expect("mk tempdir");
    TempDir(dir)
}

#[test]
fn second_invocation_fails_with_lock_contention_diagnostic() {
    let xdg = tempdir_for_test("xdg");
    let work = tempdir_for_test("work");
    let conf_path = work.0.join("cluster.local.conf");
    write_static_ip_config(&conf_path);

    // First invocation: keep it in the background long enough for the
    // second invocation to see contention. The hbird process will hold
    // the lock from `acquire_lock` until either (a) the live-mode
    // bootc/SSH error returns and the LockGuard drops, OR (b) we kill
    // it. Spawn-and-control is racy on (a); spawn-and-kill on (b) is
    // deterministic.
    //
    // To force the live-mode path to BLOCK while we observe contention,
    // we use a synthetic config that drives the orchestration past
    // `from_args` (static IPs satisfy the resolver), into the first
    // helper that surfaces live-mode-not-implemented. Even the bail-out
    // path holds the lock until run() returns — which is usually
    // sub-millisecond, faster than our second invocation's spawn.
    //
    // Solution: wrap the first hbird in a parent shell that holds the
    // lock externally via flock(1). Then the lock is held for as long
    // as the parent shell sleeps.

    // Spawn the lock holder and confirm it is genuinely holding before we
    // continue. There is a race here that made this test ~10% flaky under
    // load (measured on the KVM host): the probe below uses `flock -n` too,
    // so if a probe wins the lock before the holder does, the holder's own
    // non-blocking `flock -n` fails and the holder exits. A later probe then
    // finds the lock free. Retrying the whole holder+probe pair removes the
    // race rather than tolerating it.
    let lock_path = xdg.0.join("hbird-update-cluster.lock");
    let spawn_holder = || {
        Command::new("flock")
            // Own process group: `flock -c` runs its command in a child
            // shell that inherits the locked fd, so killing flock alone
            // leaks that shell (and its `sleep`) — the same defect this
            // suite found in acquire_lock. Kill the group instead.
            .process_group(0)
            // No `-c`: flock EXECs sleep directly, so the holder is a
            // single process with no shell wrapper. A shell wrapper defers
            // its TERM trap until the foreground `sleep` returns, which is
            // why the previous form leaked two processes per run.
            .args(["-n", &lock_path.to_string_lossy(), "sleep", "86400"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn flock holder")
    };
    let lock_held = || {
        Command::new("flock")
            .args(["-n", &lock_path.to_string_lossy(), "-c", "true"])
            .output()
            .map(|p| p.status.code() == Some(1))
            .unwrap_or(false)
    };

    let mut first = spawn_holder();
    let mut held = false;
    'outer: for _attempt in 0..5 {
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(50));
            if lock_held() {
                held = true;
                break 'outer;
            }
            // Holder lost the race and exited — restart it and re-probe.
            if matches!(first.try_wait(), Ok(Some(_))) {
                break;
            }
        }
        kill_group(&mut first);
        first = spawn_holder();
    }
    assert!(
        held,
        "flock(1) holder never took the lock across 5 attempts — test fixture bug"
    );

    // Second invocation: configure XDG_RUNTIME_DIR to the same dir and
    // expect acquire_lock to fail with the contention diagnostic.
    let second = Command::new(hbird_bin())
        .current_dir(&work.0)
        .env("XDG_RUNTIME_DIR", &xdg.0)
        // Clear HBIRD_REMOTE_NO_SUDO so we don't get a different code path.
        .env_remove("HBIRD_REMOTE_NO_SUDO")
        // Bound the blast radius if contention does NOT happen. The fixture
        // config points at TEST-NET-1 (192.0.2.0/24), which blackholes, so a
        // non-contended run would otherwise sit in the live SSH/poll path for
        // ~54s before failing — turning a clear assertion failure into what
        // looks like a hang. These budgets make the negative case fail fast
        // and loudly instead. (They are irrelevant on the happy path: the
        // lock check happens before any SSH.)
        .env("SSH_COMMAND_TIMEOUT", "3")
        .env("SSH_TIMEOUT", "3")
        .env("READY_TIMEOUT", "3")
        .env("APISERVER_TIMEOUT", "3")
        .env("DRAIN_TIMEOUT", "3s")
        .args(["update-cluster", "--config", "cluster.local.conf"])
        .output()
        .expect("spawn second hbird");

    // Was the fixture still holding when hbird ran? Distinguishes "hbird
    // failed to detect contention" from "the fixture lost its own lock".
    let lock_held_after = lock_held();
    // Tear down the lock holder (whole group — see spawn_holder).
    kill_group(&mut first);

    assert!(
        !second.status.success(),
        "second invocation should fail under lock contention; got exit 0"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("another update-cluster run is in progress"),
        "missing contention diagnostic (lock still held at assert time: {}); stderr was:\n{stderr}",
        lock_held_after,
    );
}
