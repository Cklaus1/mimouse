use super::{ConnectionError, FrontendEvent, FrontendRequest, IpcError, Request};
use std::{
    cmp::min,
    task::{Poll, ready},
    time::Duration,
};

use futures::{Stream, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio_stream::wrappers::LinesStream;

#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(unix)]
type IpcStream = UnixStream;

#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
#[cfg(windows)]
type IpcStream = NamedPipeClient;

pub struct AsyncFrontendEventReader {
    lines_stream: LinesStream<BufReader<ReadHalf<IpcStream>>>,
}

pub struct AsyncFrontendRequestWriter {
    tx: WriteHalf<IpcStream>,
    seq: u64,
}

impl Stream for AsyncFrontendEventReader {
    type Item = Result<FrontendEvent, IpcError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let line = ready!(self.lines_stream.poll_next_unpin(cx));
        let event = line.map(|l| {
            l.map_err(Into::<IpcError>::into)
                .and_then(|l| serde_json::from_str(l.as_str()).map_err(|e| e.into()))
        });
        Poll::Ready(event)
    }
}

impl AsyncFrontendRequestWriter {
    /// send a request, returns its sequence number
    pub async fn request(&mut self, request: FrontendRequest) -> Result<u64, IpcError> {
        self.seq += 1;
        let request = Request {
            seq: self.seq,
            request,
        };
        let mut json = serde_json::to_string(&request).expect("serializable request");
        log::debug!("requesting: {json}");
        json.push('\n');
        self.tx.write_all(json.as_bytes()).await?;
        Ok(self.seq)
    }
}

pub async fn connect_async(
    timeout: Option<Duration>,
) -> Result<(AsyncFrontendEventReader, AsyncFrontendRequestWriter), ConnectionError> {
    let stream = if let Some(duration) = timeout {
        tokio::select! {
            s = wait_for_service() => s?,
            _ = tokio::time::sleep(duration) => return Err(ConnectionError::Timeout),
        }
    } else {
        wait_for_service().await?
    };
    let (rx, tx) = tokio::io::split(stream);
    let lines_stream = LinesStream::new(BufReader::new(rx).lines());
    Ok((
        AsyncFrontendEventReader { lines_stream },
        AsyncFrontendRequestWriter { tx, seq: 0 },
    ))
}

/// wait for the mimouse socket to come online
#[cfg(unix)]
async fn wait_for_service() -> Result<UnixStream, ConnectionError> {
    let socket_path = super::default_socket_path()?;
    let mut duration = Duration::from_millis(10);
    loop {
        if let Ok(stream) = UnixStream::connect(&socket_path).await {
            break Ok(stream);
        }
        // a signaling mechanism or inotify could be used to
        // improve this
        tokio::time::sleep(exponential_back_off(&mut duration)).await;
    }
}

/// wait for the mimouse pipe to come online and check its owner
#[cfg(windows)]
async fn wait_for_service() -> Result<NamedPipeClient, ConnectionError> {
    let name = super::windows::pipe_name()?;
    let mut duration = Duration::from_millis(10);
    loop {
        // not found: service not running; busy: all instances in use
        match ClientOptions::new().open(&name) {
            Ok(pipe) => {
                super::windows::verify_owner(&pipe)?;
                break Ok(pipe);
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => break Err(e.into()),
            Err(_) => {}
        }
        tokio::time::sleep(exponential_back_off(&mut duration)).await;
    }
}

fn exponential_back_off(duration: &mut Duration) -> Duration {
    let new = duration.saturating_mul(2);
    *duration = min(new, Duration::from_secs(1));
    *duration
}
