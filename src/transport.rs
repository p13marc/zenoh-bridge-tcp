//! Transport abstraction for the bridge.
//!
//! Provides `TransportReader` and `TransportWriter` traits that unify TCP and
//! WebSocket I/O, allowing the core bridge loop to be generic over the
//! underlying transport.

use futures_util::{SinkExt, StreamExt};
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

/// Reader half of a bridge transport.
///
/// Implementations extract bytes from the underlying transport, handling any
/// framing (e.g., WebSocket message boundaries) internally.
pub trait TransportReader: Send + 'static {
    /// Read the next chunk of data.
    ///
    /// * Returns non-empty `Vec<u8>` on success.
    /// * Returns empty `Vec` on clean EOF / connection close.
    /// * Returns `Err` on I/O error.
    ///
    /// `buf_size` is a hint for the maximum read size; WebSocket
    /// implementations may ignore it.
    fn read_data(
        &mut self,
        buf_size: usize,
    ) -> impl std::future::Future<Output = io::Result<Vec<u8>>> + Send;
}

/// Writer half of a bridge transport.
pub trait TransportWriter: Send + 'static {
    /// Write `data` to the transport (with any framing applied internally).
    fn write_data(
        &mut self,
        data: &[u8],
    ) -> impl std::future::Future<Output = io::Result<()>> + Send;

    /// Send an EOF signal through the transport.
    ///
    /// TCP: no-op (EOF is signaled by closing the socket).
    /// WebSocket: sends a Close frame.
    fn send_eof(&mut self) -> impl std::future::Future<Output = io::Result<()>> + Send {
        async { Ok(()) }
    }

    /// Shut down the write half.
    fn shutdown(&mut self) -> impl std::future::Future<Output = io::Result<()>> + Send;
}

// ---------------------------------------------------------------------------
// TCP implementation
// ---------------------------------------------------------------------------

/// TCP reader wrapping any `AsyncReadExt` type.
pub struct TcpReader<R> {
    inner: R,
    buffer: Vec<u8>,
}

impl<R> TcpReader<R> {
    pub fn new(inner: R, buf_size: usize) -> Self {
        Self {
            inner,
            buffer: vec![0u8; buf_size],
        }
    }
}

impl<R: AsyncReadExt + Unpin + Send + 'static> TransportReader for TcpReader<R> {
    async fn read_data(&mut self, _buf_size: usize) -> io::Result<Vec<u8>> {
        let n = self.inner.read(&mut self.buffer).await?;
        if n == 0 {
            Ok(Vec::new())
        } else {
            Ok(self.buffer[..n].to_vec())
        }
    }
}

/// TCP writer wrapping any `AsyncWriteExt` type.
pub struct TcpWriter<W> {
    inner: W,
}

impl<W> TcpWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<W: AsyncWriteExt + Unpin + Send + 'static> TransportWriter for TcpWriter<W> {
    async fn write_data(&mut self, data: &[u8]) -> io::Result<()> {
        self.inner.write_all(data).await?;
        self.inner.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown().await
    }
}

// ---------------------------------------------------------------------------
// WebSocket implementation
// ---------------------------------------------------------------------------

/// WebSocket reader wrapping any `Stream` that yields tungstenite `Message`s.
pub struct WsReader<S> {
    inner: S,
}

impl<S> WsReader<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S> TransportReader for WsReader<S>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
{
    async fn read_data(&mut self, _buf_size: usize) -> io::Result<Vec<u8>> {
        loop {
            match self.inner.next().await {
                None => return Ok(Vec::new()),
                Some(Err(e)) => {
                    return Err(io::Error::other(format!("WebSocket read error: {}", e)));
                }
                Some(Ok(msg)) => match msg {
                    Message::Binary(data) => return Ok(data.to_vec()),
                    Message::Text(text) => return Ok(text.as_bytes().to_vec()),
                    Message::Close(_) => return Ok(Vec::new()),
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                },
            }
        }
    }
}

/// WebSocket writer wrapping any `Sink` that accepts tungstenite `Message`s.
pub struct WsWriter<S> {
    inner: S,
}

impl<S> WsWriter<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S> TransportWriter for WsWriter<S>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin + Send + 'static,
{
    async fn write_data(&mut self, data: &[u8]) -> io::Result<()> {
        self.inner
            .send(Message::Binary(data.to_vec().into()))
            .await
            .map_err(|e| io::Error::other(format!("WebSocket send error: {}", e)))
    }

    async fn send_eof(&mut self) -> io::Result<()> {
        self.inner
            .send(Message::Close(None))
            .await
            .map_err(|e| io::Error::other(format!("WebSocket close error: {}", e)))
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.inner
            .close()
            .await
            .map_err(|e| io::Error::other(format!("WebSocket shutdown error: {}", e)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn test_tcp_reader_reads_data() {
        let (mut client, server) = duplex(1024);
        client.write_all(b"hello").await.unwrap();
        drop(client); // close to signal EOF after data

        let mut reader = TcpReader::new(server, 1024);
        let data = reader.read_data(1024).await.unwrap();
        assert_eq!(data, b"hello");

        let eof = reader.read_data(1024).await.unwrap();
        assert!(eof.is_empty());
    }

    #[tokio::test]
    async fn test_tcp_reader_eof_on_close() {
        let (_client, server) = duplex(1024);
        drop(_client);

        let mut reader = TcpReader::new(server, 1024);
        let data = reader.read_data(1024).await.unwrap();
        assert!(data.is_empty());
    }

    #[tokio::test]
    async fn test_tcp_writer_writes_data() {
        let (client, mut server) = duplex(1024);
        let mut writer = TcpWriter::new(client);

        writer.write_data(b"hello").await.unwrap();
        writer.shutdown().await.unwrap();

        let mut buf = vec![0u8; 1024];
        let n = server.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello");
    }
}
