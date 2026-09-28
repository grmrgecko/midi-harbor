//! Every request the graphical interface can make, the command line can make too (FR-039c,
//! SC-014b).
//!
//! A headless machine has only the command line, so anything reachable from the GUI alone would
//! be something a headless user cannot do. Each call in the contract takes its own request type,
//! so the request types a crate names are the calls it can make.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::Path;

/// Collects every `…Request` type named in the Rust files under `dir`.
fn requests_named_in(dir: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("a source directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("a source file");
            for word in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
                let named_request = word.len() > "Request".len()
                    && word.ends_with("Request")
                    && word.starts_with(|c: char| c.is_ascii_uppercase());
                if named_request {
                    found.insert(word.to_owned());
                }
            }
        }
    }
    found
}

/// Locks that every contract request the GUI names, the command line names too.
///
/// A headless machine has only the command line, so a call only the window can make is one a
/// headless user cannot. Scanning the sources rather than listing calls by hand means a new
/// window feature fails here until the command line gains it.
#[test]
fn every_gui_request_is_reachable_from_the_command_line() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let gui = requests_named_in(&root.join("crates/gui/src"));
    let cli = requests_named_in(&root.join("crates/cli/src"));

    // An empty set would pass trivially, which is how this test would stop meaning anything if
    // the interface moved its calls somewhere else.
    assert!(
        gui.len() >= 5,
        "found only {gui:?} in the GUI; has its client moved?"
    );
    let missing: Vec<&String> = gui.difference(&cli).collect();
    assert!(
        missing.is_empty(),
        "the GUI can make these requests and the command line cannot: {missing:?}"
    );
}
