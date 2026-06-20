use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::hints::HintStyle;
use crate::session::{Request, Session};
use crate::ui::UiHandle;

/// Socket path: `$XDG_RUNTIME_DIR/peck.sock`, falling back to the temp dir.
fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("peck.sock")
}

/// Bind the socket and serve activations until interrupted.
pub async fn run(ui: UiHandle) -> Result<()> {
    let session = Session::new().await?;
    let path = socket_path();

    bind_fresh(&path).await?;
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to bind peck socket at {}", path.display()))?;
    tracing::info!(socket = %path.display(), "peck daemon ready; bind `peck activate` to a key");

    let result = serve(&listener, |stream| handle(&session, &ui, stream)).await;

    let _ = std::fs::remove_file(&path);
    result
}

/// Accept connections and run one activation at a time.
///
/// Repeated activations are refused immediately rather than queued in the
/// socket backlog.
async fn serve<H, Fut>(listener: &UnixListener, mut serve_one: H) -> Result<()>
where
    H: FnMut(UnixStream) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to install SIGTERM handler")?;

    loop {
        // Wait for a request that starts the next activation.
        let stream = tokio::select! {
            accepted = listener.accept() => accepted.context("accept on peck socket failed")?.0,
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received Ctrl-C; shutting down");
                return Ok(());
            }
            _ = term.recv() => {
                tracing::info!("received SIGTERM; shutting down");
                return Ok(());
            }
        };

        let activation = serve_one(stream);
        tokio::pin!(activation);
        loop {
            tokio::select! {
                result = &mut activation => {
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "activation failed");
                    }
                    break;
                }
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.context("accept on peck socket failed")?;
                    tracing::debug!("activation already in progress; refusing request");
                    let mut scratch = Vec::new();
                    let _ = stream.read_to_end(&mut scratch).await;
                    let _ = stream
                        .write_all(b"busy: peck is already serving an activation\n")
                        .await;
                    let _ = stream.shutdown().await;
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
}

/// Handle one client connection
async fn handle(session: &Session, ui: &UiHandle, mut stream: UnixStream) -> Result<()> {
    let mut request = Vec::new();
    stream
        .read_to_end(&mut request)
        .await
        .context("failed to read activation request")?;

    let (mode, style) = parse_request(&request);

    match session.activate(ui, mode, style).await {
        Ok(outcome) => {
            tracing::info!(%outcome, "activation complete");
            stream
                .write_all(format!("{outcome}\n").as_bytes())
                .await
                .context("failed to write activation result to client")?;
            Ok(())
        }
        Err(e) => {
            let _ = stream.write_all(format!("error: {e}\n").as_bytes()).await;
            Err(e)
        }
    }
}

/// Parse a request line: `<mode> [hint-style]`
fn parse_request(raw: &[u8]) -> (Request, HintStyle) {
    let mut tokens = std::str::from_utf8(raw).unwrap_or("").split_whitespace();
    let mode = tokens
        .next()
        .and_then(|t| Request::from_str(t, false).ok())
        .unwrap_or_default();
    let style = tokens
        .next()
        .and_then(|t| HintStyle::from_str(t, false).ok())
        .unwrap_or_default();
    (mode, style)
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
pub async fn activate(mode: Request, style: HintStyle) -> Result<()> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "no peck daemon at {} — start one with `peck daemon`",
            path.display()
        )
    })?;

    stream
        .write_all(format!("{mode} {style}\n").as_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{mpsc, Notify};

    /// Connect to `path`, send a request, and return the daemon's full reply.
    async fn request(path: &Path) -> String {
        let mut stream = UnixStream::connect(path).await.expect("connect");
        stream.write_all(b"\n").await.expect("write request");
        stream.shutdown().await.ok();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).await.expect("read reply");
        resp
    }

    /// Socket lines carry `<mode> [hint-style]` (e.g. `panel short`)
    /// Unrecognised or missing options default gracefully.
    #[test]
    fn parse_request_lines() {
        assert_eq!(
            parse_request(b"panel short\n"),
            (Request::Panel, HintStyle::Short)
        );
        // Invalid hint-style degrades to default
        assert_eq!(
            parse_request(b"left_click uniform\n"),
            (Request::LeftClick, HintStyle::default())
        );
        // Missing hint-style degrades to default
        assert_eq!(
            parse_request(b"warp\n"),
            (Request::Warp, HintStyle::default())
        );
        // Missing mode+hint-style degrades to default
        assert_eq!(
            parse_request(b"\n"),
            (Request::default(), HintStyle::default())
        );
        // Invalid mode+hint-style degrades to default
        assert_eq!(
            parse_request(b"nonsense nonsense\n"),
            (Request::default(), HintStyle::default())
        );
    }

    /// While one activation is in flight, a second connection must be refused
    #[tokio::test]
    async fn refuses_overlapping_activations() {
        let path = std::env::temp_dir().join(format!("peck-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind test socket");

        // Stand-in for the on-screen overlay.
        let started_count = Arc::new(AtomicUsize::new(0));
        let (start_tx, mut start_rx) = mpsc::unbounded_channel::<()>();
        let release = Arc::new(Notify::new());

        let server = {
            let started_count = started_count.clone();
            let release = release.clone();
            tokio::spawn(async move {
                serve(&listener, move |mut stream| {
                    let started_count = started_count.clone();
                    let start_tx = start_tx.clone();
                    let release = release.clone();
                    async move {
                        let mut buf = Vec::new();
                        stream.read_to_end(&mut buf).await?;
                        started_count.fetch_add(1, Ordering::SeqCst);
                        start_tx.send(()).ok();
                        release.notified().await;
                        stream.write_all(b"done\n").await?;
                        Ok(())
                    }
                })
                .await
            })
        };

        // First activation starts and parks on `release`.
        let path_a = path.clone();
        let a = tokio::spawn(async move { request(&path_a).await });
        start_rx.recv().await.expect("activation A started");

        // Second activation while the first is active: must be refused immediately and
        // not start a handler.
        let busy = request(&path).await;
        assert!(
            busy.starts_with("busy:"),
            "overlapping activation should be refused, got {busy:?}"
        );
        assert_eq!(
            started_count.load(Ordering::SeqCst),
            1,
            "refused activation must not start a second activation"
        );

        // Finish the first activation.
        release.notify_one();
        assert_eq!(a.await.expect("join A"), "done\n");

        // Third activation after the first completes: the loop recovered and serves it.
        let path_c = path.clone();
        let c = tokio::spawn(async move { request(&path_c).await });
        tokio::time::timeout(Duration::from_secs(1), start_rx.recv())
            .await
            .expect("activation C should start after A completes")
            .expect("activation C started");
        release.notify_one();
        assert_eq!(c.await.expect("join C"), "done\n");
        assert_eq!(started_count.load(Ordering::SeqCst), 2);

        server.abort();
        let _ = std::fs::remove_file(&path);
    }
}
