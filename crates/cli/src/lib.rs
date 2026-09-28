//! The command-line interface.
//!
//! Every action available in the graphical interface has an equivalent here, because the GUI is
//! optional at build time and must never be the only way to do something.

pub mod client;
pub mod commands;
pub mod exit;
pub mod output;
pub mod service_cmd;

pub use commands::{Cli, Command};
pub use exit::ExitCode;

pub mod run;
