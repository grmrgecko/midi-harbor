//! Scratch directories for the daemon's tests.

use std::path::PathBuf;
use std::sync::Once;

/// Returns this test file's scratch root under the system temporary directory, emptied the first
/// time it is asked for in each run.
///
/// A test cannot remove its own directory when it ends, because the daemon's watchers keep the
/// daemon alive until the process exits. Emptying the root when a run starts keeps scratch from
/// piling up across runs instead: only what the latest run left behind remains. Two runs of the
/// same test file at once would empty each other's, which nothing here does.
pub fn scratch(root: &str) -> PathBuf {
    static EMPTIED: Once = Once::new();
    let path = std::env::temp_dir().join(root);
    EMPTIED.call_once(|| {
        let _ = std::fs::remove_dir_all(&path);
    });
    path
}

/// Returns paths under `root` whose configuration keeps sessions off the network.
///
/// A test's sessions were announced to every machine on the network while it ran, and a long
/// soak's for a day. Allowed to go unused, since not every test file starts a session.
#[allow(dead_code)]
pub fn quiet(root: PathBuf) -> midi_harbor_core::paths::Paths {
    let paths = midi_harbor_core::paths::Paths::rooted_at(root);
    let _ = std::fs::create_dir_all(paths.config_dir());
    let _ = std::fs::write(
        paths.config_file(),
        "preferences:\n  advertise_sessions: false\n",
    );
    paths
}
