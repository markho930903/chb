#[cfg(not(target_os = "macos"))]
compile_error!("chb currently supports macOS only");

mod config;
mod fsutil;
mod macos;
mod settings;
mod web;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{RouteKind, reconcile};
use macos::{
    install_services, start_services, status, stop_services, ui, uninstall, uninstall_services,
    watch,
};
use settings::{Overrides, Settings};
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

const INSTALLER_URL: &str = "https://raw.githubusercontent.com/markho930903/chb/main/install.sh";

#[derive(Parser)]
#[command(
    name = "chb",
    version,
    about = "Bridge Codex Desktop -> Headroom -> selected provider."
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    cc_db: Option<PathBuf>,
    #[arg(long, global = true, value_parser = clap::value_parser!(u16).range(1..))]
    web_port: Option<u16>,
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
    Sync,
    Watch,
    #[command(hide = true)]
    Serve,
    Ui {
        #[arg(long)]
        no_open: bool,
    },
    #[command(name = "rm")]
    Remove,
    Update,
    Uninstall {
        #[arg(long, help = "Also uninstall Headroom and delete its local data")]
        headroom: bool,
    },
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
    if matches!(&cli.command, Command::Update) {
        update()?;
        return Ok(0);
    }

    let settings = Settings::load(Overrides {
        config_path: cli.config,
        cc_db_path: cli.cc_db,
        web_port: cli.web_port,
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
        Command::Serve => web::serve(&settings)?,
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
        Command::Update => unreachable!(),
        Command::Uninstall { headroom } => {
            uninstall(&settings, headroom)?;
            println!(
                "Uninstalled CHB{}.",
                if headroom { " and Headroom" } else { "" }
            );
        }
    }
    Ok(0)
}

fn update() -> Result<()> {
    let download = process::Command::new("/usr/bin/curl")
        .args(["-fsSL", INSTALLER_URL])
        .output()
        .context("failed to download update installer")?;
    if !download.status.success() {
        let stderr = String::from_utf8_lossy(&download.stderr);
        anyhow::bail!(
            "failed to download update installer: {}",
            stderr.trim().to_owned()
        );
    }

    let mut installer = process::Command::new("/bin/sh")
        .arg("-s")
        .stdin(process::Stdio::piped())
        .spawn()
        .context("failed to start update installer")?;
    let write_result = installer
        .stdin
        .take()
        .context("update installer stdin is unavailable")?
        .write_all(&download.stdout)
        .context("failed to pass update installer to shell");
    let status = installer.wait().context("failed to run update installer")?;
    write_result?;
    if !status.success() {
        anyhow::bail!("update installer failed with {status}");
    }
    Ok(())
}

fn print_status(data: &macos::RuntimeStatus) {
    let checks = [
        ("Headroom", data.headroom_ready),
        ("Provider config", data.provider_config_ready()),
        ("Headroom LaunchAgent", data.proxy_service_loaded),
        ("Bridge LaunchAgent", data.bridge_service_loaded),
        ("CHB Web LaunchAgent", data.web_service_loaded),
        ("Codex route", data.config.route == RouteKind::Bridged),
    ];
    for (name, ok) in checks {
        println!("{:<4}  {name}", if ok { "OK" } else { "FAIL" });
    }
    println!(
        "      provider={} route={} upstream={}",
        data.config.provider.as_deref().unwrap_or("None"),
        data.config.route,
        data.config.upstream.as_deref().unwrap_or("None")
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
    fn commands_parse() {
        assert!(matches!(
            Cli::try_parse_from(["chb", "sync"]).unwrap().command,
            Command::Sync
        ));
        assert!(matches!(
            Cli::try_parse_from(["chb", "ui", "--no-open"])
                .unwrap()
                .command,
            Command::Ui { no_open: true }
        ));
        assert!(matches!(
            Cli::try_parse_from(["chb", "serve"]).unwrap().command,
            Command::Serve
        ));
        assert!(matches!(
            Cli::try_parse_from(["chb", "rm"]).unwrap().command,
            Command::Remove
        ));
        assert!(matches!(
            Cli::try_parse_from(["chb", "update"]).unwrap().command,
            Command::Update
        ));
        assert!(matches!(
            Cli::try_parse_from(["chb", "uninstall"]).unwrap().command,
            Command::Uninstall { headroom: false }
        ));
        assert!(matches!(
            Cli::try_parse_from(["chb", "uninstall", "--headroom"])
                .unwrap()
                .command,
            Command::Uninstall { headroom: true }
        ));
    }
}
