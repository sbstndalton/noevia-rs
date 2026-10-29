//! A streamed reply body fed by a task that reads the sidecar: the task stops (and closes the
//! sidecar connection) when the receiving side is dropped, which is what a client that goes away
//! does, as `res.on('close')` aborted the fetch in core `workspace-export.cjs`.

use bytes::Bytes;
use http_body::{Body, Frame};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

/// Why a stream ended early. The connection is then cut, as `res.destroy()` did: a download that
/// stops short must look unfinished to the client, never complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamError(pub &'static str);

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for StreamError {}

pub struct ChannelBody {
    rx: mpsc::Receiver<Result<Bytes, StreamError>>,
}

impl ChannelBody {
    pub fn new(rx: mpsc::Receiver<Result<Bytes, StreamError>>) -> Self {
        Self { rx }
    }
}

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = StreamError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.rx
            .poll_recv(cx)
            .map(|item| item.map(|chunk| chunk.map(Frame::data)))
    }
}
