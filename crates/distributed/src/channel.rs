//! Transport-neutral agent channel and bounded NDJSON implementation.

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::protocol::{WIRE_PROTOCOL, WireFrame};

/// Default maximum JSON bytes in one control frame.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Control-channel failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ChannelError {
    /// Underlying stream I/O failed.
    #[error("control channel I/O: {0}")]
    Io(#[from] std::io::Error),
    /// A frame was not valid JSON for the current schema.
    #[error("invalid control frame: {0}")]
    Json(#[from] serde_json::Error),
    /// A peer attempted an unsupported wire version.
    #[error("unsupported distributed wire protocol `{got}` (expected `{expected}`)")]
    UnsupportedProtocol {
        /// Identifier received from the peer.
        got: String,
        /// Identifier implemented by this binary.
        expected: &'static str,
    },
    /// A frame exceeded the configured fixed bound.
    #[error("control frame exceeds {limit} bytes")]
    FrameTooLarge {
        /// Accepted byte limit.
        limit: usize,
    },
    /// A peer closed in the middle of a frame.
    #[error("control channel ended before the frame newline")]
    UnterminatedFrame,
    /// Blank lines are not protocol frames.
    #[error("empty control frame")]
    EmptyFrame,
}

/// Asynchronous, ordered, reliable frame channel.
///
/// The trait is deliberately transport-neutral. v0.2 uses
/// [`NdjsonChannel`] over an OpenSSH child's pipes; a future WSS carrier can
/// implement the same contract.
#[async_trait]
pub trait AgentChannel: Send {
    /// Send one complete frame.
    async fn send(&mut self, frame: &WireFrame) -> Result<(), ChannelError>;
    /// Receive the next frame, or `None` after a clean frame boundary EOF.
    async fn receive(&mut self) -> Result<Option<WireFrame>, ChannelError>;
    /// Flush and close the outbound half.
    async fn close(&mut self) -> Result<(), ChannelError>;
}

/// Newline-delimited JSON channel over independent async read/write halves.
pub struct NdjsonChannel<R, W> {
    reader: BufReader<R>,
    writer: W,
    max_frame_bytes: usize,
    read_buffer: Vec<u8>,
}

impl<R, W> NdjsonChannel<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Construct a channel with [`DEFAULT_MAX_FRAME_BYTES`].
    pub fn new(reader: R, writer: W) -> Self {
        Self::with_max_frame_bytes(reader, writer, DEFAULT_MAX_FRAME_BYTES)
    }

    /// Construct a channel with an explicit non-zero frame bound.
    pub fn with_max_frame_bytes(reader: R, writer: W, max_frame_bytes: usize) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer,
            max_frame_bytes: max_frame_bytes.max(1),
            read_buffer: Vec::new(),
        }
    }

    /// Recover the buffered reader and writer.
    pub fn into_inner(self) -> (BufReader<R>, W) {
        (self.reader, self.writer)
    }

    async fn receive_inner(&mut self) -> Result<Option<WireFrame>, ChannelError> {
        self.read_buffer.clear();
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                return if self.read_buffer.is_empty() {
                    Ok(None)
                } else {
                    Err(ChannelError::UnterminatedFrame)
                };
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(available.len(), |position| position + 1);
            let payload_len = take.saturating_sub(usize::from(newline.is_some()));
            if self
                .read_buffer
                .len()
                .checked_add(payload_len)
                .is_none_or(|length| length > self.max_frame_bytes)
            {
                return Err(ChannelError::FrameTooLarge {
                    limit: self.max_frame_bytes,
                });
            }
            self.read_buffer
                .extend_from_slice(&available[..payload_len]);
            self.reader.consume(take);

            if newline.is_some() {
                if self.read_buffer.last() == Some(&b'\r') {
                    self.read_buffer.pop();
                }
                if self.read_buffer.is_empty() {
                    return Err(ChannelError::EmptyFrame);
                }
                let frame: WireFrame = serde_json::from_slice(&self.read_buffer)?;
                if frame.protocol != WIRE_PROTOCOL {
                    return Err(ChannelError::UnsupportedProtocol {
                        got: frame.protocol,
                        expected: WIRE_PROTOCOL,
                    });
                }
                return Ok(Some(frame));
            }
        }
    }
}

#[async_trait]
impl<R, W> AgentChannel for NdjsonChannel<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    async fn send(&mut self, frame: &WireFrame) -> Result<(), ChannelError> {
        if frame.protocol != WIRE_PROTOCOL {
            return Err(ChannelError::UnsupportedProtocol {
                got: frame.protocol.clone(),
                expected: WIRE_PROTOCOL,
            });
        }
        let encoded = serde_json::to_vec(frame)?;
        if encoded.len() > self.max_frame_bytes {
            return Err(ChannelError::FrameTooLarge {
                limit: self.max_frame_bytes,
            });
        }
        self.writer.write_all(&encoded).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn receive(&mut self) -> Result<Option<WireFrame>, ChannelError> {
        self.receive_inner().await
    }

    async fn close(&mut self) -> Result<(), ChannelError> {
        self.writer.shutdown().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, duplex, split};

    use super::*;
    use crate::protocol::{HelloFrame, SupportedScenario, WireMessage};

    fn hello() -> WireFrame {
        WireFrame::new(WireMessage::Hello(HelloFrame {
            agent_name: None,
            binary_version: "0.2.0".to_owned(),
            scenarios: vec![SupportedScenario::Sustained],
            max_concurrency: 100,
        }))
    }

    #[tokio::test]
    async fn round_trips_one_frame() {
        let (left, right) = duplex(4096);
        let (left_read, left_write) = split(left);
        let (right_read, right_write) = split(right);
        let mut sender = NdjsonChannel::new(left_read, left_write);
        let mut receiver = NdjsonChannel::new(right_read, right_write);

        sender.send(&hello()).await.unwrap();
        let received = receiver.receive().await.unwrap().unwrap();
        assert!(matches!(received.message, WireMessage::Hello(_)));
    }

    #[tokio::test]
    async fn rejects_unknown_wire_protocol() {
        let (mut raw, right) = duplex(4096);
        let (right_read, right_write) = split(right);
        let mut receiver = NdjsonChannel::new(right_read, right_write);
        raw.write_all(
            br#"{"protocol":"mcp-loadtest-dist/99","type":"hello","payload":{"agent_name":"east","binary_version":"0.2.0","scenarios":["sustained"],"max_concurrency":1}}"#,
        )
        .await
        .unwrap();
        raw.write_all(b"\n").await.unwrap();

        assert!(matches!(
            receiver.receive().await,
            Err(ChannelError::UnsupportedProtocol { .. })
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_frame_without_waiting_for_newline() {
        let (mut raw, right) = duplex(4096);
        let (right_read, right_write) = split(right);
        let mut receiver = NdjsonChannel::with_max_frame_bytes(right_read, right_write, 16);
        raw.write_all(b"12345678901234567").await.unwrap();

        assert!(matches!(
            receiver.receive().await,
            Err(ChannelError::FrameTooLarge { limit: 16 })
        ));
    }
}
