mod atspi;
mod daemon;
mod geometry;
mod hints;
mod instrument;
mod niri;
mod pointer;
mod session;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};

use hints::HintStyle;
use session::{Request, Session};

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
        /// Gesture to dispatch on the selected target, or `panel`.
        #[arg(long, value_enum, default_value_t = Request::default())]
        mode: Request,

        /// Hint label style.
        #[arg(long, value_enum, default_value_t = HintStyle::default())]
        hints: HintStyle,
    },

    /// Trigger the daemon
    Activate {
        /// Gesture to dispatch on the selected target, or `panel`.
        #[arg(long, value_enum, default_value_t = Request::default())]
        mode: Request,

        /// Hint label style.
        #[arg(long, value_enum, default_value_t = HintStyle::default())]
        hints: HintStyle,
    },

    /// Print `role | name | object-path` for each actionable element and exit.
    DumpTree,

    /// Print each element with corrected global physical coords and exit.
    DumpCoords,
}

fn main() -> Result<()> {
    instrument::init();
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon => ui::run(|ui| async move { daemon::run(ui).await }),
        Command::Oneshot { mode, hints } => ui::run(move |ui| async move {
            let session = Session::new().await?;
            println!("{}", session.activate(&ui, mode, hints).await?);
            Ok(())
        }),
        Command::Activate { mode, hints } => block_on(daemon::activate(mode, hints)),
        Command::DumpTree => block_on(async { dump_tree_cmd(&Session::new().await?).await }),
        Command::DumpCoords => block_on(async { dump_coords_cmd(&Session::new().await?).await }),
    }
}

fn block_on<Fut: std::future::Future<Output = Result<()>>>(fut: Fut) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(fut)
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
