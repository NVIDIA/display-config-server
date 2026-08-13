// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! dcs-tool — Display Config Server configuration tool.
//!
//! # Usage
//!
//! ```text
//! dcs-tool show
//! dcs-tool apply --config /etc/dcs/config.yaml
//! dcs-tool apply --display 1 --mode 1920x1080@60000
//! dcs-tool apply --display 1 --mode 1920x1080@60000 --display 2 --mode 1920x1080@60000
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};

use dcs_config::{
    config::{Config, DisplayConfig, ModeConfig, TopologyConfig},
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
                    let marker = if mode.current {
                        "* "
                    } else {
                        "  "
                    };
                    let tag = match (mode.current, mode.preferred) {
                        (true, _)      => " [current]",
                        (false, true)  => " [preferred]",
                        _              => "",
                    };
                    println!(
                        "{}{}x{}@{}mHz{}",
                        marker, mode.width, mode.height, mode.refresh_mhz, tag
                    );
                }
            }
        }
    }

    println!();
    println!(
        "QuadroSync: {}",
        if quadro_sync { "supported" } else { "not detected" }
    );

    Ok(())
}

fn cmd_apply(args: ApplyArgs) -> anyhow::Result<()> {
    if let Some(path) = &args.config {
        cmd_apply_yaml(path)
    } else if !args.display.is_empty() {
        cmd_apply_cli(&args.display, &args.mode)
    } else {
        anyhow::bail!("specify --config FILE or one or more --display N --mode WxH@R pairs")
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

fn cmd_apply_cli(displays: &[u32], modes: &[String]) -> anyhow::Result<()> {
    if displays.len() != modes.len() {
        anyhow::bail!(
            "--display and --mode must appear in pairs (got {} displays, {} modes)",
            displays.len(),
            modes.len()
        );
    }

    let display_configs: Vec<DisplayConfig> = displays
        .iter()
        .zip(modes.iter())
        .map(|(number, mode_str)| {
            Ok(DisplayConfig {
                number: *number,
                mode: Some(parse_mode(mode_str)?),
            })
        })
        .collect::<anyhow::Result<_>>()?;

    let topology = TopologyConfig { display: display_configs };
    let mut client = connect()?;
    client.apply(&topology)?;
    println!("Configuration applied successfully.");
    Ok(())
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
}
