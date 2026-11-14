//! hx's harness core as a library: the supervisor, the Luau VM, the journal, the policy and the effects they own.
//! The `hx` binary (`src/main.rs`) is its command line; other binaries in this package build workflows on the same
//! primitives.
#[cfg(not(target_os = "linux"))]
compile_error!("hx supports Linux only");

/// `print!` that returns `Result` instead of panicking: see `write_stdout`. Defined before the modules so they can use
/// it too.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => { $crate::write_stdout(format_args!($($arg)*)) };
}

/// `println!` that returns `Result` instead of panicking: see `write_stdout`.
#[macro_export]
macro_rules! outln {
    ($($arg:tt)*) => { $crate::write_stdout(format_args!("{}\n", format_args!($($arg)*))) };
}

pub mod atomic_file;
pub mod batch;
pub mod builtins;
pub mod confine;
pub mod controller;
pub mod dirs;
pub mod effects;
pub mod eventlog;
pub mod history;
pub mod host;
pub mod llm;
pub mod mcp;
pub mod modules;
pub mod net;
pub mod packages;
pub mod policy;
pub mod prims;
pub mod procs;
pub mod supervisor;
pub mod tools;
pub mod trust;
pub mod values;
pub mod vm;
pub mod workflow;
pub mod workflow_runtime;

#[cfg(test)]
mod tests;

/// stdout was closed by its reader (`hx run ... | head -1`): hx stops writing and exits quietly.
#[derive(Debug)]
pub struct ClosedStdout;
impl std::fmt::Display for ClosedStdout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stdout is closed")
    }
}
impl std::error::Error for ClosedStdout {}

/// The exit status after stdout was closed: what a process killed by SIGPIPE reports to its shell. hx keeps SIGPIPE
/// ignored (Rust's default) because it writes to child pipes, such as an MCP server's stdin, that may close.
pub const CLOSED_STDOUT_STATUS: u8 = 128 + libc::SIGPIPE as u8;

/// Write to stdout and flush, for the `out!` and `outln!` macros. A closed stdout is `ClosedStdout`, which `main`
/// turns into a quiet exit; `print!` would panic instead, and the panic hook aborts.
pub fn write_stdout(text: std::fmt::Arguments<'_>) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    match stdout.write_fmt(text).and_then(|()| stdout.flush()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Err(ClosedStdout.into()),
        result => Ok(result.context("cannot write to stdout")?),
    }
}
