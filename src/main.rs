#[cfg(not(target_os = "macos"))]
compile_error!("codex-headroom-bridge currently supports macOS only");

mod config;
mod fsutil;
mod macos;
mod settings;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{RouteKind, reconcile};
use macos::{
    install_services, start_services, status, stop_services, ui, uninstall_services, watch,
};
use settings::{Overrides, Settings};
use std::env;
use std::path::{Path, PathBuf};
use std::process;

#[derive(Parser)]
#[command(
    name = "chb",
    version,
    about = "Bridge Codex Desktop -> Headroom -> CC Switch."
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    cc_db: Option<PathBuf>,
    #[arg(long, global = true, value_parser = clap::value_parser!(u16).range(1..))]
    headroom_port: Option<u16>,
    #[arg(long, global = true, value_parser = clap::value_parser!(u16).range(1..))]
    cc_port: Option<u16>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Install {
        #[arg(long)]
        headroom_bin: Option<PathBuf>,
        #[arg(long)]
        bridge_bin: Option<PathBuf>,
    },
    Start,
    Stop,
    Status,
    Doctor,
    #[command(name = "sync", visible_alias = "reconcile")]
    Sync,
    Watch,
    Ui {
        #[arg(long)]
        no_open: bool,
    },
    #[command(name = "rm", visible_alias = "uninstall")]
    Remove,
}

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("chb: {error:#}");
            1
        }
    };
    process::exit(code);
}

fn run() -> Result<i32> {
    let cli = Cli::parse();
    let settings = Settings::load(Overrides {
        config_path: cli.config,
        cc_db_path: cli.cc_db,
        headroom_port: cli.headroom_port,
        cc_port: cli.cc_port,
    })?;

    match cli.command {
        Command::Install {
            headroom_bin,
            bridge_bin,
        } => {
            let headroom_bin = headroom_bin
                .or_else(|| find_program("headroom"))
                .context("headroom executable not found")?;
            let bridge_bin = bridge_bin
                .or_else(|| find_program("chb"))
                .or_else(|| env::current_exe().ok())
                .context("chb executable not found")?;
            let backup = install_services(&settings, &headroom_bin, &bridge_bin)?;
            println!("Installed. Config backup: {}", backup.display());
        }
        Command::Start => start_services(&settings)?,
        Command::Stop => stop_services(&settings)?,
        Command::Status | Command::Doctor => {
            let doctor = matches!(cli.command, Command::Doctor);
            let data = status(&settings);
            print_status(&data);
            if doctor && !data.healthy() {
                return Ok(1);
            }
        }
        Command::Sync => {
            println!(
                "{}",
                if reconcile(&settings, true)? {
                    "updated"
                } else {
                    "already bridged"
                }
            );
        }
        Command::Watch => watch(&settings)?,
        Command::Ui { no_open } => {
            let url = ui(&settings, no_open)?;
            println!("Dashboard: {url}");
        }
        Command::Remove => {
            uninstall_services(&settings)?;
            println!(
                "Removed LaunchAgents. Backups remain in {}",
                settings.state_dir.join("backups").display()
            );
        }
    }
    Ok(0)
}

fn print_status(data: &macos::RuntimeStatus) {
    let checks = [
        ("Headroom", data.headroom_ready),
        ("CC Switch proxy", data.cc_switch_ready),
        ("CC Switch Codex takeover", data.cc_switch_takeover),
        ("Headroom LaunchAgent", data.proxy_service_loaded),
        ("Bridge LaunchAgent", data.bridge_service_loaded),
        ("Codex route", data.config.route == RouteKind::Bridged),
    ];
    for (name, ok) in checks {
        println!("{:<4}  {name}", if ok { "OK" } else { "FAIL" });
    }
    println!(
        "      provider={} route={}",
        data.config.provider.as_deref().unwrap_or("None"),
        data.config.route
    );
}

fn find_program(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_commands_and_legacy_aliases_parse() {
        assert!(matches!(
            Cli::try_parse_from(["chb", "ui", "--no-open"])
                .unwrap()
                .command,
            Command::Ui { no_open: true }
        ));
        assert!(matches!(
            Cli::try_parse_from(["chb", "reconcile"]).unwrap().command,
            Command::Sync
        ));
        assert!(matches!(
            Cli::try_parse_from(["chb", "uninstall"]).unwrap().command,
            Command::Remove
        ));
    }
}
