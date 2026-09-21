// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! dcs-tool — Display Config Server configuration tool.
//!
//! dcs-tool is the command line configuration tool used to control displays owned
//! by Display Config Server. dcs-tool allows for dynamic configuration via a set
//! of CLI arguments and also supports reading from a configuration file.
//!
//! # Usage
//!
//! ```text
//! dcs-tool show
//! dcs-tool apply --config /etc/dcs/config.yaml
//! dcs-tool apply --display 1 --mode 1920x1080@60000
//! dcs-tool apply --display 1 --mode 1920x1080@60000 --display 2 --mode 1920x1080@60000
//! dcs-tool apply --display 1 --mode 1920x1080@60000 --display 2 --mode 1920x1080@60000 \
//!                --qs-role 1=server --qs-role 2=client --qs-enable
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};

use dcs_client::{
    config::{
        Config, DisplayConfig, HouseSyncMode, ModeConfig, QuadroSyncConfig, QuadroSyncPolarity,
        QuadroSyncRole, TopologyConfig,
    },
    connection::connect,
};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "dcs-tool",
    about = "Display Config Server configuration tool",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print current DCS display configuration
    Show,
    /// Apply a display configuration
    Apply(ApplyArgs),
}

#[derive(Args)]
struct ApplyArgs {
    /// YAML config file (mutually exclusive with --display/--mode)
    #[arg(long, conflicts_with_all = &["display", "mode"])]
    config: Option<PathBuf>,

    /// Display number to configure (may be repeated, paired with --mode)
    #[arg(long = "display")]
    display: Vec<u32>,

    /// Mode as WxH@R in mHz, e.g. 1920x1080@60000 (paired with --display)
    #[arg(long = "mode")]
    mode: Vec<String>,

    /// QuadroSync role as N=disabled|server|client (may be repeated)
    #[arg(long = "qs-role", conflicts_with = "config")]
    qs_role: Vec<String>,

    /// QuadroSync sync delay
    #[arg(long = "qs-sync-delay", conflicts_with = "config")]
    qs_sync_delay: Option<u32>,

    /// QuadroSync polarity: rising_edge|falling_edge|both_edges
    #[arg(long = "qs-polarity", conflicts_with = "config")]
    qs_polarity: Option<String>,

    /// QuadroSync house sync mode: disabled|input|output
    #[arg(long = "qs-house-sync", conflicts_with = "config")]
    qs_house_sync: Option<String>,

    /// Enable QuadroSync sync
    #[arg(long = "qs-enable", conflicts_with_all = &["config", "qs_disable"])]
    qs_enable: bool,

    /// Disable QuadroSync sync
    #[arg(long = "qs-disable", conflicts_with = "config")]
    qs_disable: bool,
}

impl ApplyArgs {
    /// True when any board-level QuadroSync flag was given
    /// (`--qs-sync-delay`, `--qs-polarity`, `--qs-house-sync`,
    /// `--qs-enable`, `--qs-disable`).
    fn has_board_settings(&self) -> bool {
        self.qs_sync_delay.is_some()
            || self.qs_polarity.is_some()
            || self.qs_house_sync.is_some()
            || self.qs_enable
            || self.qs_disable
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Show => cmd_show(),
        Command::Apply(args) => cmd_apply(args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {:#}", e);
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Subcommand implementations
// ---------------------------------------------------------------------------

fn cmd_show() -> anyhow::Result<()> {
    let mut client = connect()?;
    let quadro_sync = client.quadro_sync_supported();
    let outputs = client.enumerate_outputs()?;

    if outputs.is_empty() {
        println!("No displays found.");
    } else {
        for output in &outputs {
            let dev = output.dev_t;
            let num = output.display_number;

            // Header line: display number and device
            println!("Display {:2}  (dev {})", num, dev);

            if output.modes.is_empty() {
                println!("  (no modes)");
            } else {
                for mode in &output.modes {
                    let current = output.current_mode_id == Some(mode.id);
                    let preferred = output.preferred_mode_id == Some(mode.id);
                    let marker = if current { "  * " } else { "    " };
                    let tag = match (current, preferred) {
                        (true, _) => " [current]",
                        (false, true) => " [preferred]",
                        _ => "",
                    };
                    println!(
                        "{}{}x{}@{}mHz{}",
                        marker, mode.width, mode.height, mode.refresh_mhz, tag
                    );
                }
            }

            if let Some(qs) = &output.quadro_sync {
                let role = match qs.role {
                    QuadroSyncRole::Disabled => "disabled",
                    QuadroSyncRole::Server => "server",
                    QuadroSyncRole::Client => "client",
                };
                let sync = if qs.sync_active { "active" } else { "inactive" };
                match qs.board {
                    Some(board) => println!("  QuadroSync role: {}, sync: {}, board: {}", role, sync, board),
                    None => println!("  QuadroSync role: {}, sync: {}", role, sync),
                }
            }
        }
    }

    println!();
    println!(
        "QuadroSync: {}",
        if quadro_sync {
            "supported"
        } else {
            "not detected"
        }
    );

    Ok(())
}

fn cmd_apply(args: ApplyArgs) -> anyhow::Result<()> {
    if let Some(path) = &args.config {
        cmd_apply_yaml(path)
    } else if !args.display.is_empty() || !args.qs_role.is_empty() || args.has_board_settings() {
        cmd_apply_cli(&args)
    } else {
        anyhow::bail!(
            "specify --config FILE, one or more --display N --mode WxH@R pairs, \
             or --qs-* flags"
        )
    }
}

fn cmd_apply_yaml(path: &PathBuf) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    let config: Config = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse YAML in '{}'", path.display()))?;

    let mut client = connect()?;
    for topology in &config.topology {
        client.apply(topology)?;
    }
    println!("Configuration applied successfully.");
    Ok(())
}

fn cmd_apply_cli(args: &ApplyArgs) -> anyhow::Result<()> {
    if args.display.len() != args.mode.len() {
        anyhow::bail!(
            "--display and --mode must appear in pairs (got {} displays, {} modes)",
            args.display.len(),
            args.mode.len()
        );
    }

    let mut display_configs: Vec<DisplayConfig> = args
        .display
        .iter()
        .zip(args.mode.iter())
        .map(|(number, mode_str)| {
            Ok(DisplayConfig {
                number: *number,
                mode: Some(parse_mode(mode_str)?),
                quadro_sync_role: None,
            })
        })
        .collect::<anyhow::Result<_>>()?;

    // Attach roles to existing display entries, or add role-only entries for
    // displays that appear in --qs-role but not in --display.
    for role_str in &args.qs_role {
        let (number, role) = parse_qs_role(role_str)?;
        match display_configs.iter_mut().find(|d| d.number == number) {
            Some(d) => d.quadro_sync_role = Some(role),
            None => display_configs.push(DisplayConfig {
                number,
                mode: None,
                quadro_sync_role: Some(role),
            }),
        }
    }

    let quadro_sync = if args.has_board_settings() {
        Some(QuadroSyncConfig {
            sync_delay: args.qs_sync_delay,
            polarity: args
                .qs_polarity
                .as_deref()
                .map(parse_qs_polarity)
                .transpose()?,
            house_sync_mode: args
                .qs_house_sync
                .as_deref()
                .map(parse_qs_house_sync)
                .transpose()?,
            sync_enable: if args.qs_enable {
                Some(true)
            } else if args.qs_disable {
                Some(false)
            } else {
                None
            },
        })
    } else {
        None
    };

    let topology = TopologyConfig {
        display: display_configs,
        quadro_sync,
    };
    let mut client = connect()?;
    client.apply(&topology)?;
    println!("Configuration applied successfully.");
    Ok(())
}

/// Parse a `--qs-role` value in the form `N=disabled|server|client`.
fn parse_qs_role(s: &str) -> anyhow::Result<(u32, QuadroSyncRole)> {
    let (num_str, role_str) = s
        .split_once('=')
        .with_context(|| format!("invalid --qs-role '{s}': expected N=role (e.g. 1=server)"))?;
    let number: u32 = num_str
        .parse()
        .with_context(|| format!("invalid display number in '{s}'"))?;
    let role = match role_str {
        "disabled" => QuadroSyncRole::Disabled,
        "server" => QuadroSyncRole::Server,
        "client" => QuadroSyncRole::Client,
        other => {
            anyhow::bail!("invalid QuadroSync role '{other}': expected disabled, server, or client")
        }
    };
    Ok((number, role))
}

/// Parse a `--qs-polarity` value.
fn parse_qs_polarity(s: &str) -> anyhow::Result<QuadroSyncPolarity> {
    match s {
        "rising_edge" => Ok(QuadroSyncPolarity::RisingEdge),
        "falling_edge" => Ok(QuadroSyncPolarity::FallingEdge),
        "both_edges" => Ok(QuadroSyncPolarity::BothEdges),
        other => anyhow::bail!(
            "invalid polarity '{other}': expected rising_edge, falling_edge, or both_edges"
        ),
    }
}

/// Parse a `--qs-house-sync` value.
fn parse_qs_house_sync(s: &str) -> anyhow::Result<HouseSyncMode> {
    match s {
        "disabled" => Ok(HouseSyncMode::Disabled),
        "input" => Ok(HouseSyncMode::Input),
        "output" => Ok(HouseSyncMode::Output),
        other => {
            anyhow::bail!("invalid house sync mode '{other}': expected disabled, input, or output")
        }
    }
}

/// Parse a mode string in the form `WxH@R` where R is in mHz.
///
/// Example: `"1920x1080@60000"` → `ModeConfig { width: 1920, height: 1080, refresh_mhz: 60000 }`
fn parse_mode(s: &str) -> anyhow::Result<ModeConfig> {
    let (res, refresh_str) = s
        .split_once('@')
        .with_context(|| format!("invalid mode '{s}': expected WxH@R (e.g. 1920x1080@60000)"))?;
    let (width_str, height_str) = res
        .split_once('x')
        .with_context(|| format!("invalid mode '{s}': expected WxH@R (e.g. 1920x1080@60000)"))?;
    Ok(ModeConfig {
        width: width_str
            .parse()
            .with_context(|| format!("invalid width in '{s}'"))?,
        height: height_str
            .parse()
            .with_context(|| format!("invalid height in '{s}'"))?,
        refresh_mhz: refresh_str
            .strip_suffix("mHz")
            .unwrap_or(refresh_str)
            .parse()
            .with_context(|| format!("invalid refresh rate in '{s}'"))?,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_valid() {
        let m = parse_mode("1920x1080@60000").unwrap();
        assert_eq!(m.width, 1920);
        assert_eq!(m.height, 1080);
        assert_eq!(m.refresh_mhz, 60000);
    }

    #[test]
    fn parse_mode_missing_at() {
        assert!(parse_mode("1920x1080").is_err());
    }

    #[test]
    fn parse_mode_missing_x() {
        assert!(parse_mode("1920-1080@60000").is_err());
    }

    #[test]
    fn parse_mode_non_numeric_width() {
        assert!(parse_mode("axb@60000").is_err());
    }

    #[test]
    fn parse_mode_mhz_suffix() {
        let m = parse_mode("1920x1080@60000mHz").unwrap();
        assert_eq!(m.refresh_mhz, 60000);
    }

    #[test]
    fn parse_qs_role_valid() {
        assert_eq!(
            parse_qs_role("1=server").unwrap(),
            (1, QuadroSyncRole::Server)
        );
        assert_eq!(
            parse_qs_role("2=client").unwrap(),
            (2, QuadroSyncRole::Client)
        );
        assert_eq!(
            parse_qs_role("3=disabled").unwrap(),
            (3, QuadroSyncRole::Disabled)
        );
    }

    #[test]
    fn parse_qs_role_invalid() {
        assert!(parse_qs_role("1=admin").is_err());
        assert!(parse_qs_role("server").is_err());
        assert!(parse_qs_role("x=server").is_err());
    }

    #[test]
    fn parse_qs_polarity_valid() {
        assert_eq!(
            parse_qs_polarity("rising_edge").unwrap(),
            QuadroSyncPolarity::RisingEdge
        );
        assert_eq!(
            parse_qs_polarity("falling_edge").unwrap(),
            QuadroSyncPolarity::FallingEdge
        );
        assert_eq!(
            parse_qs_polarity("both_edges").unwrap(),
            QuadroSyncPolarity::BothEdges
        );
        assert!(parse_qs_polarity("sideways").is_err());
    }

    #[test]
    fn parse_qs_house_sync_valid() {
        assert_eq!(
            parse_qs_house_sync("disabled").unwrap(),
            HouseSyncMode::Disabled
        );
        assert_eq!(parse_qs_house_sync("input").unwrap(), HouseSyncMode::Input);
        assert_eq!(
            parse_qs_house_sync("output").unwrap(),
            HouseSyncMode::Output
        );
        assert!(parse_qs_house_sync("both").is_err());
    }

    fn empty_apply_args() -> ApplyArgs {
        ApplyArgs {
            config: None,
            display: Vec::new(),
            mode: Vec::new(),
            qs_role: Vec::new(),
            qs_sync_delay: None,
            qs_polarity: None,
            qs_house_sync: None,
            qs_enable: false,
            qs_disable: false,
        }
    }

    #[test]
    fn has_board_settings_detects_each_flag() {
        assert!(!empty_apply_args().has_board_settings());

        let mut args = empty_apply_args();
        args.qs_sync_delay = Some(5);
        assert!(args.has_board_settings());

        let mut args = empty_apply_args();
        args.qs_polarity = Some("rising_edge".into());
        assert!(args.has_board_settings());

        let mut args = empty_apply_args();
        args.qs_house_sync = Some("input".into());
        assert!(args.has_board_settings());

        let mut args = empty_apply_args();
        args.qs_enable = true;
        assert!(args.has_board_settings());

        let mut args = empty_apply_args();
        args.qs_disable = true;
        assert!(args.has_board_settings());
    }

    #[test]
    fn board_settings_only_invocation_parses_and_is_accepted() {
        let cli = Cli::try_parse_from(["dcs-tool", "apply", "--qs-enable"]).unwrap();
        let Command::Apply(args) = cli.command else {
            panic!("expected apply subcommand");
        };
        assert!(args.display.is_empty());
        assert!(args.qs_role.is_empty());
        // The cmd_apply gate accepts this invocation via has_board_settings.
        assert!(args.has_board_settings());
    }
}
