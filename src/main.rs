#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
//! Manifold (`manifold`) — Cross-Platform Dual-Pane File Manager & Remote Transfer Engine.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use manifold_config::CredentialStore;
use manifold_ui::ManifoldAppState;
use manifold_vfs::{FileSystemBackend, LocalFsBackend};

#[derive(Debug, Parser)]
#[command(
    name = "manifold",
    version,
    about = "Manifold — Cross-Platform Dual-Pane File Manager & Transfer Engine"
)]
struct Cli {
    /// Optional initial directory for the left pane
    #[arg(long)]
    left_dir: Option<PathBuf>,

    /// Optional initial directory for the right pane
    #[arg(long)]
    right_dir: Option<PathBuf>,

    /// Print the initialized Manifold workspace state as JSON and exit
    #[arg(long)]
    dump_state_json: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// List directory entries using the Manifold VFS engine
    Ls {
        #[arg(default_value = ".")]
        path: String,
    },
}

fn main() -> Result<()> {
    let home_var = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
    if let Ok(home) = home_var {
        let at_root = std::env::current_dir()
            .map(|p| p == std::path::Path::new("/") || p == std::path::Path::new("C:\\"))
            .unwrap_or(true);
        if at_root && std::path::Path::new(&home).is_dir() {
            let _ = std::env::set_current_dir(&home);
        }
    }
    let mut clean_args = Vec::new();
    for arg in std::env::args_os() {
        if !arg.to_string_lossy().starts_with("-psn_") {
            clean_args.push(arg);
        }
    }
    let cli = Cli::parse_from(clean_args);
    let _keyring = CredentialStore::new();
    let vfs = LocalFsBackend::new();

    if let Some(Commands::Ls { path }) = cli.command {
        let entries = vfs.list_directory(&path)?;
        for entry in entries {
            println!(
                "{}\t{}\t{}",
                entry.permissions_matrix().to_symbolic_string(),
                entry.formatted_size(),
                entry.display_name()
            );
        }
        return Ok(());
    }

    if cli.dump_state_json {
        let mut state = ManifoldAppState::new_reference_workspace();
        let l_idx = state.left_active_tab;
        let l_tab = state.left_tabs.get_mut(l_idx);
        if let (Some(left), Some(tab)) = (&cli.left_dir, l_tab) {
            tab.current_path = left.to_string_lossy().to_string();
        }
        let r_idx = state.right_active_tab;
        let r_tab = state.right_tabs.get_mut(r_idx);
        if let (Some(right), Some(tab)) = (&cli.right_dir, r_tab) {
            tab.current_path = right.to_string_lossy().to_string();
        }
        println!("{}", serde_json::to_string_pretty(&state)?);
        return Ok(());
    }

    let left_ref = cli.left_dir.as_deref();
    let right_ref = cli.right_dir.as_deref();
    let state = ManifoldAppState::new_live_workspace(left_ref, right_ref);

    if let Err(err) = manifold_ui::gui::run_desktop_gui(state) {
        eprintln!("Failed to open Manifold GUI window: {err}");
        std::process::exit(1);
    }
    Ok(())
}
