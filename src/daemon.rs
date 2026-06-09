use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::pointer::Mode;
use crate::session::Session;

/// Socket path: `$XDG_RUNTIME_DIR/peck.sock`, falling back to the temp dir.
fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("peck.sock")
}

/// Bind the socket and serve activations until interrupted.
pub async fn run() -> Result<()> {
    let session = Session::new().await?;
    let path = socket_path();

    bind_fresh(&path).await?;
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to bind peck socket at {}", path.display()))?;
    tracing::info!(socket = %path.display(), "peck daemon ready; bind `peck activate` to a key");

    let result = serve(&session, &listener).await;

    let _ = std::fs::remove_file(&path);
    result
}

async fn serve(session: &Session, listener: &UnixListener) -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to install SIGTERM handler")?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = accepted.context("accept on peck socket failed")?;
                if let Err(e) = handle(session, &mut stream).await {
                    tracing::warn!(error = %e, "activation failed");
                    let _ = stream.write_all(format!("error: {e}\n").as_bytes()).await;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received Ctrl-C; shutting down");
                return Ok(());
            }
            _ = term.recv() => {
                tracing::info!("received SIGTERM; shutting down");
                return Ok(());
            }
        }
    }
}

/// Handle one client connection
async fn handle(session: &Session, stream: &mut UnixStream) -> Result<()> {
    let mut request = Vec::new();
    stream
        .read_to_end(&mut request)
        .await
        .context("failed to read activation request")?;

    // The request is a single line naming the mode; anything unrecognised
    // (including an empty line from an older client) falls back to the default.
    let mode = std::str::from_utf8(&request)
        .ok()
        .and_then(|s| Mode::from_str(s.trim(), false).ok())
        .unwrap_or_default();

    let outcome = session.activate(mode).await?;
    tracing::info!(%outcome, "activation complete");
    stream
        .write_all(format!("{outcome}\n").as_bytes())
        .await
        .context("failed to write activation result to client")?;
    Ok(())
}

/// Refuse to start if a *live* daemon already holds the socket; otherwise remove
/// a stale socket file left by a crashed daemon.
async fn bind_fresh(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if UnixStream::connect(path).await.is_ok() {
        anyhow::bail!(
            "a peck daemon is already running (socket {} is live)",
            path.display()
        );
    }
    std::fs::remove_file(path)
        .with_context(|| format!("failed to remove stale peck socket {}", path.display()))?;
    Ok(())
}

/// Connect to the running daemon and trigger one activation
pub async fn activate(mode: Mode) -> Result<()> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "no peck daemon at {} — start one with `peck daemon`",
            path.display()
        )
    })?;

    stream
        .write_all(format!("{mode}\n").as_bytes())
        .await
        .context("failed to send activation request")?;
    stream.shutdown().await.ok();

    let mut resp = String::new();
    stream
        .read_to_string(&mut resp)
        .await
        .context("failed to read activation result from daemon")?;
    print!("{resp}");
    Ok(())
}
