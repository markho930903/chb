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
    Serve {
        #[arg(long, hide = true)]
        development: bool,
        #[arg(long, hide = true)]
        headroom_bin: Option<PathBuf>,
        #[arg(long, hide = true)]
        no_open: bool,
    },
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
            let environment_ready = if doctor { print_environment() } else { true };
            if doctor && (!data.healthy() || !environment_ready) {
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
        Command::Serve {
            development,
            headroom_bin,
            no_open,
        } => {
            if development {
                let headroom_bin = headroom_bin.or_else(|| find_program("headroom")).context(
                    "headroom executable not found; install Headroom or pass --headroom-bin",
                )?;
                web::serve_development(&settings, &headroom_bin, no_open)?;
            } else {
                web::serve(&settings)?;
            }
        }
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

fn print_environment() -> bool {
    let uv = find_in_path("uv");
    let headroom = find_program("headroom");
    println!("Environment:");
    for (name, program) in [("uv", &uv), ("Headroom executable", &headroom)] {
        match program {
            Some(path) => println!("OK    {name}: {}", path.display()),
            None => println!("FAIL  {name}"),
        }
    }
    if headroom.is_none() {
        println!("      Install: uv tool install --python 3.13 \"headroom-ai[all]\"");
    }
    headroom.is_some()
}

fn find_program(name: &str) -> Option<PathBuf> {
    find_in_path(name).or_else(|| find_in_uv_tool_dir(name))
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path).find_map(|directory| executable_in(&directory, name))
    })
}

fn find_in_uv_tool_dir(name: &str) -> Option<PathBuf> {
    let uv = find_in_path("uv")?;
    let output = process::Command::new(uv)
        .args(["tool", "dir", "--bin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let directory = std::str::from_utf8(&output.stdout).ok()?.trim();
    (!directory.is_empty())
        .then(|| executable_in(Path::new(directory), name))
        .flatten()
}

fn executable_in(directory: &Path, name: &str) -> Option<PathBuf> {
    let candidate = directory.join(name);
    is_executable(&candidate).then_some(candidate)
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
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn finds_executable_in_tool_directory() {
        let temp = TempDir::new().unwrap();
        let executable = temp.path().join("headroom");
        fs::write(&executable, "#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();

        assert_eq!(executable_in(temp.path(), "headroom"), Some(executable));
    }

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
            Command::Serve {
                development: false,
                headroom_bin: None,
                no_open: false,
            }
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "chb",
                "serve",
                "--development",
                "--headroom-bin",
                "/tmp/headroom",
                "--no-open",
            ])
            .unwrap()
            .command,
            Command::Serve {
                development: true,
                headroom_bin: Some(_),
                no_open: true,
            }
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
