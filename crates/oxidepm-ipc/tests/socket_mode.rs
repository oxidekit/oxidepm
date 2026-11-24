//! The socket must be 0600 from the instant it exists. This is its own test binary because it
//! changes the process umask, which would disturb other tests running in the same process.

use oxidepm_ipc::IpcServer;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

#[tokio::test]
async fn socket_is_0600_from_creation() {
    // Watch the path from another thread while bind runs: no mode other than 0600 may ever be
    // observed, not even for an instant. The umask is made permissive first so that a
    // chmod-after-bind implementation would show 0755 in the window.
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("daemon.sock");
    let old = nix::sys::stat::umask(nix::sys::stat::Mode::empty());

    let done = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (path, done) = (path.clone(), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !done.load(Ordering::Relaxed) {
                if let Ok(meta) = std::fs::symlink_metadata(&path) {
                    seen.push(meta.permissions().mode() & 0o777);
                }
            }
            seen
        })
    };
    let server = IpcServer::bind(&path).await.unwrap();
    done.store(true, Ordering::Relaxed);
    let seen = watcher.join().unwrap();
    nix::sys::stat::umask(old);

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(
        seen.iter().all(|m| *m == 0o600),
        "modes observed during bind: {seen:?}"
    );
    drop(server);
}
