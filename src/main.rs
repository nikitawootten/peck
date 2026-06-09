mod atspi;
mod daemon;
mod geometry;
mod hints;
mod instrument;
mod niri;
mod overlay;
mod pointer;
mod session;

use anyhow::Result;
use clap::{Parser, Subcommand};

use pointer::Mode;
use session::Session;

#[derive(Parser)]
#[command(
    name = "peck",
    version,
    about = "Vimium-style mouseless navigation for Niri"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the persistent daemon.
    Daemon,

    /// Run a single activation and exit.
    Oneshot {
        /// Gesture to dispatch on the selected target.
        #[arg(long, value_enum, default_value_t = Mode::default())]
        mode: Mode,
    },

    /// Trigger the daemon
    Activate {
        /// Gesture to dispatch on the selected target.
        #[arg(long, value_enum, default_value_t = Mode::default())]
        mode: Mode,
    },

    /// Print `role | name | object-path` for each actionable element and exit.
    DumpTree,

    /// Print each element with corrected global physical coords and exit.
    DumpCoords,
}

fn main() -> Result<()> {
    instrument::init();
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(run(cli.command))
}

async fn run(command: Command) -> Result<()> {
    match command {
        Command::Oneshot { mode } => show_overlay_cmd(&Session::new().await?, mode).await,
        Command::DumpTree => dump_tree_cmd(&Session::new().await?).await,
        Command::DumpCoords => dump_coords_cmd(&Session::new().await?).await,
        Command::Daemon => daemon::run().await,
        Command::Activate { mode } => daemon::activate(mode).await,
    }
}

async fn show_overlay_cmd(session: &Session, mode: Mode) -> Result<()> {
    println!("{}", session.activate(mode).await?);
    Ok(())
}

/// Diagnostic: dump the actionable elements of the focused window.
async fn dump_tree_cmd(session: &Session) -> Result<()> {
    let elements = session.actionable_elements().await?;

    println!("{} actionable element(s):", elements.len());
    for el in &elements {
        let path = el.object.path();
        println!("{:>14?} | {:<40} | {}", el.role, el.name, path);
    }

    Ok(())
}

/// Diagnostic: dump each actionable element with corrected output-local
/// physical coordinates.
async fn dump_coords_cmd(session: &Session) -> Result<()> {
    let (geom, located) = session.located_elements().await?;

    println!(
        "window {} ({}) on output {} @ scale {}",
        geom.window_id,
        geom.app_id.as_deref().unwrap_or("?"),
        geom.output_name,
        geom.scale,
    );
    println!(
        "  output logical origin {:?}, mode {:?}; window content origin (logical) {:?}, size {:?}",
        geom.output_logical_origin, geom.output_mode, geom.content_origin, geom.window_size,
    );
    println!(
        "{} located element(s) [output-local physical px]:",
        located.len()
    );
    for (el, r) in &located {
        println!(
            "{:>14?} | {:<32} | x={:<5} y={:<5} w={:<5} h={:<5} (center {:?})",
            el.role,
            el.name,
            r.x,
            r.y,
            r.w,
            r.h,
            r.center(),
        );
    }

    Ok(())
}
