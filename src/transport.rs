//! Byte links a session runs over: a serial port, or an in-memory pipe for
//! tests.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio_serial::{SerialPort, SerialPortBuilderExt, SerialStream};

/// An async byte link with a settable line rate.
pub trait Transport: AsyncRead + AsyncWrite + Unpin + Send {
    /// Change the host side's line rate on the open link.
    fn set_baud_rate(&mut self, baud: u32) -> io::Result<()>;
    fn baud_rate(&self) -> u32;
}

/// A serial port, 8N1, no flow control on the host side.
pub struct SerialTransport {
    port: SerialStream,
    baud: u32,
}

impl SerialTransport {
    pub fn open(path: &str, baud: u32) -> io::Result<Self> {
        let mut port = tokio_serial::new(path, baud)
            .data_bits(tokio_serial::DataBits::Eight)
            .parity(tokio_serial::Parity::None)
            .stop_bits(tokio_serial::StopBits::One)
            .flow_control(tokio_serial::FlowControl::None)
            .open_native_async()
            .map_err(io::Error::from)?;
        // The PS1 transmits only while the host holds RTS up (its CTS). A pty
        // has no modem lines, so a failure here is not fatal.
        let _ = port.write_request_to_send(true);
        let _ = port.write_data_terminal_ready(true);
        port.clear(tokio_serial::ClearBuffer::All)
            .map_err(io::Error::from)?;
        Ok(SerialTransport { port, baud })
    }
}

impl Transport for SerialTransport {
    fn set_baud_rate(&mut self, baud: u32) -> io::Result<()> {
        self.port.set_baud_rate(baud).map_err(io::Error::from)?;
        self.baud = baud;
        Ok(())
    }

    fn baud_rate(&self) -> u32 {
        self.baud
    }
}

impl AsyncRead for SerialTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.port).poll_read(cx, buf)
    }
}

impl AsyncWrite for SerialTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.port).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.port).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.port).poll_shutdown(cx)
    }
}

/// One end of an in-memory byte pipe. The host end's line rate is shared
/// with whoever holds the other end (a simulator can garble bytes when the
/// two sides disagree on it).
pub struct MemTransport {
    io: DuplexStream,
    baud: Arc<AtomicU32>,
}

impl MemTransport {
    /// A connected pair: (host end, device end, the host end's rate cell).
    pub fn pair(baud: u32) -> (MemTransport, DuplexStream, Arc<AtomicU32>) {
        let (a, b) = tokio::io::duplex(1 << 16);
        let cell = Arc::new(AtomicU32::new(baud));
        (
            MemTransport {
                io: a,
                baud: cell.clone(),
            },
            b,
            cell,
        )
    }
}

impl Transport for MemTransport {
    fn set_baud_rate(&mut self, baud: u32) -> io::Result<()> {
        self.baud.store(baud, Ordering::SeqCst);
        Ok(())
    }

    fn baud_rate(&self) -> u32 {
        self.baud.load(Ordering::SeqCst)
    }
}

impl AsyncRead for MemTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for MemTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
