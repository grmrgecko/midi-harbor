//! Rendering results for people and for scripts.

use serde::Serialize;
use std::io::Write;

/// How to present results.
///
/// Under `--json` every invocation writes exactly one JSON document to stdout, as the contract
/// promises: a command's own, or a closing one carrying its outcome and whatever sentences it
/// would have printed. Commands that watch write one document per update instead.
#[derive(Debug, Default)]
pub struct Format {
    /// Emit machine-readable output instead of a table.
    pub json: bool,
    /// Suppress anything that is not an error.
    pub quiet: bool,
    /// Sentences held back under `--json`, where printing them would corrupt the document.
    held: std::sync::Mutex<Vec<String>>,
    /// Whether a document has been written, so the closing one is not added to it.
    emitted: std::sync::atomic::AtomicBool,
    /// What a command made or changed, for the closing document under `--json`.
    result: std::sync::Mutex<Option<serde_json::Value>>,
}

impl Format {
    /// Creates a format for one invocation.
    pub fn new(json: bool, quiet: bool) -> Self {
        Self {
            json,
            quiet,
            ..Self::default()
        }
    }

    /// Writes a value as JSON, for scripting.
    ///
    /// Diagnostics go to stderr throughout, so piping stdout into a parser is always safe.
    pub fn emit<T: Serialize>(&self, value: &T) {
        self.emitted
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if self.quiet {
            return;
        }
        match serde_json::to_string_pretty(value) {
            Ok(text) => println!("{text}"),
            Err(error) => eprintln!("could not encode output: {error}"),
        }
    }

    /// Writes a line of human-readable output, or holds it for the closing document under
    /// `--json`.
    pub fn line(&self, text: impl AsRef<str>) {
        if self.json {
            if let Ok(mut held) = self.held.lock() {
                held.push(text.as_ref().to_owned());
            }
            return;
        }
        if self.quiet {
            return;
        }
        println!("{}", text.as_ref());
    }

    /// Records what a command made or changed, so a script gets its identifier as well as the
    /// sentence a person reads.
    pub fn result(&self, value: serde_json::Value) {
        if let Ok(mut result) = self.result.lock() {
            *result = Some(value);
        }
    }

    /// Ends an invocation under `--json` with a document for a command that wrote none.
    ///
    /// Many commands report in a sentence, and under `--json` printed it anyway, so a script got
    /// text where it asked for JSON (FR-039d).
    pub fn finish(&self, succeeded: bool) {
        if !self.json || self.emitted.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let messages = self
            .held
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default();
        let result = self.result.lock().ok().and_then(|result| result.clone());
        let mut document = serde_json::json!({
            "ok": succeeded,
            "messages": messages,
        });
        if let (Some(result), Some(fields)) = (result, document.as_object_mut()) {
            let _ = fields.insert("result".to_owned(), result);
        }
        self.emit(&document);
    }

    /// Writes a note to stderr, so it never pollutes parsed output.
    pub fn note(&self, text: impl AsRef<str>) {
        if self.quiet {
            return;
        }
        let _ = writeln!(std::io::stderr(), "{}", text.as_ref());
    }
}

/// Renders a table with aligned columns.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.len());
            }
        }
    }

    let mut out = String::new();
    for (index, header) in headers.iter().enumerate() {
        let width = widths.get(index).copied().unwrap_or(header.len());
        out.push_str(&format!("{header:<width$}  "));
    }
    out.push('\n');

    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            let width = widths.get(index).copied().unwrap_or(cell.len());
            out.push_str(&format!("{cell:<width$}  "));
        }
        out.push('\n');
    }
    out
}
