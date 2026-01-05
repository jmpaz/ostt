//! Remote control support for ostt using a local Unix socket.
//!
//! Provides a small IPC protocol for triggering completion/cancel from external scripts.

use anyhow::{anyhow, Context};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteSignal {
    Complete,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCommand {
    Complete,
    Cancel,
    Ping,
}

impl RemoteCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteCommand::Complete => "complete",
            RemoteCommand::Cancel => "cancel",
            RemoteCommand::Ping => "ping",
        }
    }
}

pub async fn start_listener(tx: UnboundedSender<RemoteSignal>) -> anyhow::Result<PathBuf> {
    let path = socket_path();
    prepare_socket(&path).await?;

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("Failed to bind remote socket at {}", path.display()))?;

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(result) => result,
                Err(err) => {
                    tracing::warn!("Remote socket accept failed: {err}");
                    break;
                }
            };

            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(err) = handle_client(stream, tx).await {
                    tracing::warn!("Remote socket client error: {err}");
                }
            });
        }
    });

    Ok(path)
}

pub async fn send_command(command: RemoteCommand) -> anyhow::Result<()> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("Remote socket not available at {}", path.display()))?;

    let message = format!("{}\n", command.as_str());
    stream.write_all(message.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

pub fn cleanup_socket(path: &Path) {
    if let Err(err) = std::fs::remove_file(path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("Failed to remove remote socket {}: {err}", path.display());
        }
    }
}

fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("OSTT_REMOTE_SOCKET") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }

    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime_dir.trim().is_empty() {
            return PathBuf::from(runtime_dir).join("ostt.sock");
        }
    }

    std::env::temp_dir().join("ostt.sock")
}

async fn prepare_socket(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        match UnixStream::connect(path).await {
            Ok(_) => {
                return Err(anyhow!(
                    "Remote socket already in use at {}",
                    path.display()
                ));
            }
            Err(_) => {
                std::fs::remove_file(path).with_context(|| {
                    format!("Failed to remove stale remote socket at {}", path.display())
                })?;
            }
        }
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create remote socket directory {}", parent.display())
            })?;
        }
    }

    Ok(())
}

async fn handle_client(
    stream: UnixStream,
    tx: UnboundedSender<RemoteSignal>,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 {
        return Ok(());
    }

    match line.trim().to_ascii_lowercase().as_str() {
        "complete" => {
            let _ = tx.send(RemoteSignal::Complete);
        }
        "cancel" => {
            let _ = tx.send(RemoteSignal::Cancel);
        }
        "ping" => {}
        other => {
            tracing::debug!("Ignoring remote command: {}", other);
        }
    }

    Ok(())
}
