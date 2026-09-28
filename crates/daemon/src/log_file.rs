//! The daemon's own log file, for service managers that keep none.
//!
//! systemd sends a unit's output to the journal, but launchd discards it unless the agent names a
//! file, and a file launchd writes to grows without limit. The daemon writes the file itself
//! instead, and rolls it over at a size cap, keeping one previous file beside it.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// How large the log grows before it is rolled over.
///
/// With the previous file kept beside it, the log takes at most twice this on disk.
pub const LOG_CAP: u64 = 10 * 1024 * 1024;

/// A log file that is rolled over to `<name>.1` once it passes a size cap.
#[derive(Debug)]
pub struct RollingFile {
    path: PathBuf,
    file: File,
    written: u64,
    cap: u64,
}

impl RollingFile {
    /// Opens the log for appending, creating it and its directory when missing.
    pub fn open(path: &Path, cap: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = append(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            written,
            cap,
        })
    }

    /// Returns where the previous log is kept once rolled over.
    pub fn previous(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(".1");
        PathBuf::from(name)
    }

    /// Moves the current log aside, replacing the previous one, and starts an empty log.
    fn roll_over(&mut self) -> io::Result<()> {
        self.file.flush()?;
        std::fs::rename(&self.path, Self::previous(&self.path))?;
        self.file = append(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RollingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // A line that would take the log past its cap starts the next file, so no line is split
        // across two. A failed roll-over keeps writing to the current file rather than losing
        // the line.
        let incoming = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if self.written > 0 && self.written.saturating_add(incoming) > self.cap {
            let _ = self.roll_over();
        }
        let wrote = self.file.write(buf)?;
        self.written = self
            .written
            .saturating_add(u64::try_from(wrote).unwrap_or(u64::MAX));
        Ok(wrote)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Opens a file for appending, creating it when missing.
fn append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the on-disk shape of the log: it and its directory are created and appended to
    /// across opens, a line that would take it past the cap starts a new file with exactly one
    /// previous file kept as `<name>.1`, the size already on disk counts after a restart, and no
    /// line is ever split or lost. An empty log is never rolled over, which would replace the
    /// previous file with nothing.
    #[test]
    fn the_log_rolls_over_at_its_cap_keeping_one_previous_file() {
        struct Case {
            name: &'static str,
            file: &'static str,
            left_on_disk: Option<&'static str>,
            cap: u64,
            opens: Vec<Vec<&'static str>>,
            want: &'static str,
            want_previous: Option<&'static str>,
        }
        let cases = [
            Case {
                name: "a missing log and directory are created and appended to across opens",
                file: "nested/daemon.log",
                left_on_disk: None,
                cap: 1024,
                opens: vec![vec!["first\n"], vec!["second\n"]],
                want: "first\nsecond\n",
                want_previous: None,
            },
            Case {
                // Each line is 7 bytes, and 7 + 7 = 14 passes the cap of 10, so every line after
                // the first starts a new file.
                name: "a full log rolls over keeping only the one before the current",
                file: "daemon.log",
                left_on_disk: None,
                cap: 10,
                opens: vec![vec!["aaaaaa\n", "bbbbbb\n", "cccccc\n"]],
                want: "cccccc\n",
                want_previous: Some("bbbbbb\n"),
            },
            Case {
                // 18 bytes from the last run plus 9 now is 27, past the cap of 20.
                name: "the size already on disk counts after a restart",
                file: "daemon.log",
                left_on_disk: Some("from the last run\n"),
                cap: 20,
                opens: vec![vec!["this run\n"]],
                want: "this run\n",
                want_previous: Some("from the last run\n"),
            },
            Case {
                name: "a line longer than the cap is written whole into an empty log",
                file: "daemon.log",
                left_on_disk: None,
                cap: 4,
                opens: vec![vec!["longer than four\n"]],
                want: "longer than four\n",
                want_previous: None,
            },
        ];

        for (index, case) in cases.into_iter().enumerate() {
            let dir = std::env::temp_dir().join(format!("mh-log-{index}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let path = dir.join(case.file);
            if let Some(content) = case.left_on_disk {
                std::fs::create_dir_all(&dir).expect("the scratch directory is created");
                std::fs::write(&path, content).expect("the last run's log is written");
            }
            for lines in &case.opens {
                let mut log = RollingFile::open(&path, case.cap).expect("the log opens");
                for line in lines {
                    log.write_all(line.as_bytes()).expect("the line is written");
                }
                log.flush().expect("the log flushes");
            }

            assert_eq!(
                std::fs::read_to_string(&path).expect("the current log is readable"),
                case.want,
                "{}: the current log holds the wrong lines",
                case.name
            );
            assert_eq!(
                std::fs::read_to_string(RollingFile::previous(&path)).ok(),
                case.want_previous.map(str::to_owned),
                "{}: the previous log holds the wrong lines",
                case.name
            );
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
