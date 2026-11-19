//! The `nohrs` binary: parses CLI arguments, runs headless `config` subcommands
//! when requested, and otherwise launches the gpui desktop application.

// A window program, not a console one: otherwise Windows opens a console
// beside the window (full of the log) whenever the app is started from the
// Start menu, a shortcut or Explorer. Debug builds keep theirs, which is
// where a developer reads the log.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod cli;

use clap::Parser;

fn main() {
    // Before parsing: clap prints help, version and argument errors itself.
    // Only for these text commands: opening a folder from a terminal must not
    // pour the app's log into it.
    if std::env::args().nth(1).is_some_and(|first| {
        matches!(
            first.as_str(),
            "config" | "open" | "current-path" | "help" | "-h" | "--help" | "-V" | "--version"
        )
    }) {
        attach_parent_console();
    }
    let mut cli = cli::Cli::parse();

    if matches!(cli.command, Some(cli::Command::CurrentPath)) {
        match nohrs_pages::ipc::current_path() {
            Ok(path) => println!("{path}"),
            Err(error) => {
                eprintln!("LumaFiles: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if let Some(cli::Command::Open { path }) = &cli.command {
        if !path.exists() {
            eprintln!("LumaFiles: path does not exist: {}", path.display());
            std::process::exit(1);
        }
        cli.path = Some(path.clone());
        cli.command = None;
    }

    // `config` subcommands run headless and exit without opening a window.
    if let Some(cli::Command::Config { action }) = &cli.command {
        nohrs_core::telemetry::logging::init_logging();
        std::process::exit(cli::run_config_command(action, &cli));
    }

    if let Some(cli::Command::ForgetListary) = &cli.command {
        nohrs_core::telemetry::logging::init_logging();
        let code = match nohrs_pages::listary::disable() {
            Ok(()) => 0,
            Err(error) => {
                tracing::error!("could not give Listary's folders back to Explorer: {error}");
                1
            }
        };
        std::process::exit(code);
    }

    // One window per user: a running instance takes the path (or just comes
    // forward), which is what Listary and "Open with" expect of a file
    // manager. Only when nobody answers does this process open a window.
    if cli.command.is_none() {
        let handed_over = match &cli.path {
            Some(path) => nohrs_pages::ipc::request_open(path),
            // Started at sign-in while one already runs: nothing to show.
            None if cli.background => nohrs_pages::ipc::is_running(),
            None => nohrs_pages::ipc::request_show(),
        };
        if handed_over {
            return;
        }
    }

    if let Err(error) = nohrs_core::config::paths::prepare_storage() {
        eprintln!("could not prepare application storage: {error}");
    }

    app::NohrsApp::run(&cli);

    // Here, after the event loop, rather than in a gpui quit hook: the window
    // is gone and nothing of the app is left for the installer to wait on.
    nohrs_pages::updater::install_pending_on_exit();
}

/// A window program has no console of its own, so a `config` command typed
/// in a terminal would print nowhere: it writes to the terminal it came from.
/// Started any other way there is none to attach to, and nothing to print.
#[cfg(all(windows, not(debug_assertions)))]
#[allow(unsafe_code)]
fn attach_parent_console() {
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    // SAFETY: a plain Win32 call with a constant argument; a failure (no
    // parent console) only means there is nowhere to print.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        tracing::debug!("no parent console to attach to");
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn attach_parent_console() {}
