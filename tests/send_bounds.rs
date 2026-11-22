//! Integration tests that guard the `Sophon` async public API against
//! `Send + 'static` regressions when used on a multi-threaded tokio runtime.
//!
//! `elysiae` (and any other real GTK/Tokio consumer) drives irmin by spawning
//! each `Sophon` method's future on a multi-threaded runtime via
//! `tokio::spawn` (or an equivalent helper requiring `Future + Send +
//! 'static`). A borrow of `&self`, `&str`, `&Path`, or `&DownloadHandle` that
//! survives across an `await` point makes the returned future `Send` only for
//! one specific lifetime — not for *all* lifetimes — so the spawn fails with
//! the classic "implementation of `Send` is not general enough" error.
//!
//! These tests spawn each public `Sophon` method's future on a fresh
//! multi-threaded runtime and assert that the future type-checks as
//! `Send + 'static`. They do not exercise network or disk paths: the methods
//! fail fast (with `SophonError::NoGameManifest` or a transport error) before
//! reaching any I/O, but the spawn itself is the regression guard.

#![cfg(test)]

use std::path::PathBuf;
use std::sync::Arc;

use irmin_rs::{DownloadHandle, Sophon, SophonProgress, game_installer::SophonError};

/// Build a `Sophon` pointed at a nonexistent install dir and game id.
///
/// The point is not to complete the operation — it is to spawn the returned
/// future on a multi-threaded runtime and assert its `Send + 'static` bounds
/// survive the type-checker. Every method below fails fast (network or
/// manifest error) without touching disk beyond reading the (missing) tag.
fn sophon_fixture() -> Sophon {
    Sophon::builder(
        "test-game-id",
        PathBuf::from("/tmp/irmin-send-tests-nonexistent"),
    )
    .vo_lang("en-US")
    .build()
}

/// Cheap progress callback satisfying `ProgressFn` (`Send + Sync + 'static`).
fn noop_progress() -> impl Fn(SophonProgress) + Send + Sync + 'static {
    move |_event: SophonProgress| {}
}

/// Spawn a future on a fresh multi-threaded runtime and block until it
/// resolves. Returns the future's output.
///
/// `tokio::runtime::Runtime::spawn` requires `Future + Send + 'static`;
/// if the `Sophon` API regresses and returns a future that is only `Send` for
/// one specific borrow lifetime, this function fails to compile with the
/// same "implementation of `Send` is not general enough" error that real
/// consumers see at their build time.
fn spawn_on_multi_thread<F>(f: F) -> F::Output
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build multi-thread runtime");
    let handle = rt.spawn(f);
    rt.block_on(handle).expect("spawned future panicked")
}

/// `check_update` must be spawnable on a multi-thread runtime even though it
/// borrows `&self`. The returned future is `Send + 'static` and owned by the
/// caller; the borrow of `&self` ends at the call site.
#[test]
fn check_update_future_is_send_and_static() {
    let sophon = sophon_fixture();
    let fut = sophon.check_update();
    let result = spawn_on_multi_thread(fut);
    // We don't care about the runtime error — we only care the future type-
    // checked as `Send + 'static` and was spawned. The error, if any, will be
    // a transport or manifest error, never a `NoGameManifest` from a
    // successful-but-wrong path.
    let _ = result;
}

/// `verify_integrity` must be spawnable on a multi-thread runtime. Its
/// progress callback is `'static` so the future must be too.
#[test]
fn verify_integrity_future_is_send_and_static() {
    let sophon = sophon_fixture();
    let fut = sophon.verify_integrity(noop_progress());
    let result: Result<(), SophonError> = spawn_on_multi_thread(fut);
    // `NoInstalledVersion` because the game dir does not exist; that's fine.
    let _ = result;
}

/// `download` must be spawnable on a multi-thread runtime even though it
/// borrows `&self` and `&DownloadHandle`. The returned future is
/// `Send + 'static`.
#[test]
fn download_future_is_send_and_static() {
    let sophon = sophon_fixture();
    let handle = DownloadHandle::new();
    let fut = sophon.download(&handle, noop_progress());
    let result: Result<(), SophonError> = spawn_on_multi_thread(fut);
    let _ = result;
}

/// `update` must be spawnable on a multi-thread runtime.
#[test]
fn update_future_is_send_and_static() {
    let sophon = sophon_fixture();
    let handle = DownloadHandle::new();
    let fut = sophon.update(&handle, noop_progress());
    let result: Result<(), SophonError> = spawn_on_multi_thread(fut);
    let _ = result;
}

/// `preinstall` must be spawnable on a multi-thread runtime.
#[test]
fn preinstall_future_is_send_and_static() {
    let sophon = sophon_fixture();
    let handle = DownloadHandle::new();
    let fut = sophon.preinstall(&handle, noop_progress());
    let result: Result<(), SophonError> = spawn_on_multi_thread(fut);
    let _ = result;
}

/// `apply_preinstall` must be spawnable on a multi-thread runtime even though
/// it borrows `&self`, `&str` (`preinstall_tag`), and `&DownloadHandle`.
#[test]
fn apply_preinstall_future_is_send_and_static() {
    let sophon = sophon_fixture();
    let handle = DownloadHandle::new();
    let fut = sophon.apply_preinstall("nonexistent-tag", &handle, noop_progress());
    let result: Result<(), SophonError> = spawn_on_multi_thread(fut);
    let _ = result;
}

/// The `Sophon` value itself must remain usable across a `.await` point that
/// runs concurrently on a multi-thread runtime — i.e. holding a `Sophon` in
/// one async task while another task awaits must not require the caller to
/// pin or borrow the `Sophon` for a specific lifetime.
#[test]
fn sophon_owned_can_be_moved_across_await() {
    let sophon: Arc<Sophon> = Arc::new(sophon_fixture());
    let s = Arc::clone(&sophon);
    let fut = async move {
        // Rebind `s` across an await to prove the future is `Send + 'static`
        // even when a `Sophon` is captured by an `Arc` clone.
        let inner = tokio::task::yield_now().await;
        let _ = inner;
        let _ = s.check_update().await;
    };
    let _ = spawn_on_multi_thread(fut);
}
