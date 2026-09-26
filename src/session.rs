//! A monitor session over any byte link: attach with PING, load and run a
//! program, read and write memory and registers, and wait for the program to
//! stop while collecting its console text and serving its PCDRV calls.
//! Mirrors runner-agent `monitor/session.ts` and `monitor/loader.ts`.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Instant, sleep, timeout_at};

use crate::frame::{Event, Frame, Parser, bytes_to_words, encode_frame, u32_words, word_u32, words_to_bytes};
use crate::lz4;
use crate::pcdrv::PcdrvServer;
use crate::proto::{self, *};
use crate::transport::Transport;

/// Longest PCDRV file name read out of target memory.
const PCDRV_NAME_MAX: u32 = 256;
/// How long the host PINGs at a new rate before giving up on it. The
/// monitor's window is about 1.08 s on a retail PS1.
const RATE_TRY: Duration = Duration::from_millis(700);
/// After a failed rate change, how long to wait for both of the monitor's
/// windows to close before PINGing at the old rate.
const RATE_WINDOW: Duration = Duration::from_millis(2500);

const SHORT: Duration = Duration::from_secs(2);
const BULK: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("link closed")]
    Closed,
    #[error("monitor: no reply to {0}")]
    Timeout(String),
    #[error("monitor: {what} failed: {} (0x{code:02x})", proto::error_name(*code))]
    Monitor { what: String, code: u16 },
    #[error("monitor: {0}: reply failed its checksum")]
    Checksum(String),
    #[error("monitor: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, SessionError>;

/// A STOPPED event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stop {
    pub reason: u16,
    pub epc: u32,
    pub a: u32,
    pub b: u32,
}

impl Stop {
    pub fn reason_name(&self) -> String {
        proto::stop_reason_name(self.reason)
    }
}

/// How `run_until_stop` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    /// The stop, or None at the deadline. An exit is reported as reason
    /// [`STOP_EXIT`] with the code in `a`, whichever way the monitor sent it.
    pub stop: Option<Stop>,
    /// The program's a0 at its exit break, or None if it did not exit.
    pub exit_code: Option<u32>,
}

/// How to send a program.
#[derive(Debug, Clone, Copy)]
pub struct LoadOptions {
    /// Send LZ4 slices when the monitor has [`CAP_LZ4`].
    pub lz4: bool,
    /// Longest match per sequence (see [`lz4::cap_matches`]).
    pub max_match: usize,
    /// Use LZ4 only if it shrinks the data below this fraction of its size.
    pub max_ratio: f64,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions {
            lz4: true,
            max_match: lz4::DEFAULT_MAX_MATCH,
            max_ratio: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadStats {
    pub bytes: usize,
    /// Compressed bytes sent, when the data went out as LZ4.
    pub lz4_bytes: Option<usize>,
}

type Console = Box<dyn FnMut(&[u8]) + Send>;

pub struct Session<T: Transport> {
    io: T,
    parser: Parser,
    text: Vec<u8>,
    console: Option<Console>,
    pending: VecDeque<Frame>,
    /// Protocol version from the last PONG.
    pub version: Option<u16>,
    /// Capability bits from the last full PONG.
    pub caps: u16,
    /// BIOS Fletcher-32 from the last full PONG (protocol v2).
    pub bios: Option<u32>,
    /// Log PCDRV calls to stderr.
    pub verbose: bool,
}

impl<T: Transport> Session<T> {
    pub fn new(io: T) -> Self {
        Session {
            io,
            parser: Parser::new(),
            text: Vec::new(),
            console: None,
            pending: VecDeque::new(),
            version: None,
            caps: 0,
            bios: None,
            verbose: false,
        }
    }

    pub fn transport(&mut self) -> &mut T {
        &mut self.io
    }

    /// Send console text to `sink` as it arrives instead of buffering it.
    pub fn set_console(&mut self, sink: Option<Console>) {
        self.console = sink;
        if let Some(sink) = self.console.as_mut()
            && !self.text.is_empty()
        {
            sink(&std::mem::take(&mut self.text));
        }
    }

    /// Console text buffered since the last call.
    pub fn take_text(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.text)
    }

    pub async fn send(&mut self, ty: u16, payload: &[u16]) -> Result<()> {
        self.io.write_all(&encode_frame(ty, payload)).await?;
        self.io.flush().await?;
        Ok(())
    }

    fn pump(&mut self) {
        while let Some(event) = self.parser.next_event() {
            match event {
                Event::Tty(bytes) => match self.console.as_mut() {
                    Some(sink) => sink(&bytes),
                    None => self.text.extend_from_slice(&bytes),
                },
                Event::Frame(f) => self.pending.push_back(f),
            }
        }
    }

    /// The next frame whose type is in `types`, or None at the deadline.
    /// Frames of other types that arrive meanwhile are dropped.
    pub async fn wait_frame(&mut self, types: &[u16], deadline: Instant) -> Result<Option<Frame>> {
        let mut buf = [0u8; 4096];
        loop {
            self.pump();
            if let Some(i) = self.pending.iter().position(|f| types.contains(&f.ty)) {
                let frame = self.pending.remove(i);
                self.pending.drain(..i);
                return Ok(frame);
            }
            self.pending.clear();
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match timeout_at(deadline, self.io.read(&mut buf)).await {
                Err(_) => continue,
                Ok(Ok(0)) => return Err(SessionError::Closed),
                Ok(Ok(n)) => self.parser.feed(&buf[..n]),
                Ok(Err(e)) => return Err(e.into()),
            }
        }
    }

    /// ACK, or the ERROR / timeout / checksum failure as an error.
    async fn expect_ack(&mut self, what: impl Fn() -> String, wait: Duration) -> Result<()> {
        match self.wait_frame(&[ACK, ERROR], Instant::now() + wait).await? {
            None => Err(SessionError::Timeout(what())),
            Some(f) if f.ty == ERROR => Err(SessionError::Monitor {
                what: what(),
                code: f.words.first().copied().unwrap_or(0),
            }),
            Some(f) if !f.ok => Err(SessionError::Checksum(what())),
            Some(_) => Ok(()),
        }
    }

    /// PING until PONG, or false after `wait`. A PONG with only the version
    /// (the SET_BAUD window PONGs) leaves caps and BIOS as they were.
    pub async fn ping(&mut self, wait: Duration, payload: &[u16]) -> Result<bool> {
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            self.send(PING, payload).await?;
            let slot = deadline.min(Instant::now() + Duration::from_millis(500));
            if let Some(pong) = self.wait_frame(&[PONG], slot).await?
                && pong.ok
            {
                if let Some(&v) = pong.words.first() {
                    self.version = Some(v);
                }
                if pong.words.len() > 1 {
                    self.caps = pong.words[1];
                }
                if pong.words.len() > 3 {
                    self.bios = Some(word_u32(&pong.words, 2));
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Write `data` at `addr` in plain LOAD frames of 8 KiB, each ACKed
    /// before the next.
    pub async fn load_raw(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        self.bulk_write(LOAD, "LOAD", addr, data).await
    }

    /// Write `data` at `addr` in WRITE_MEM frames of 8 KiB.
    pub async fn write_mem(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        self.bulk_write(WRITE_MEM, "WRITE_MEM", addr, data).await
    }

    async fn bulk_write(&mut self, ty: u16, name: &str, addr: u32, data: &[u8]) -> Result<()> {
        for (i, chunk) in data.chunks(CHUNK_BYTES).enumerate() {
            let at = addr.wrapping_add((i * CHUNK_BYTES) as u32);
            let mut payload = u32_words(&[at, chunk.len() as u32]);
            payload.extend(bytes_to_words(chunk));
            self.send(ty, &payload).await?;
            self.expect_ack(|| format!("{name} at 0x{at:08x}"), BULK).await?;
        }
        Ok(())
    }

    /// Write `raw_len` bytes at `addr` from the LZ4 block `comp`, sent as
    /// consecutive slices in LOAD|LZ4 frames, each ACKed before the next.
    pub async fn load_lz4(&mut self, addr: u32, raw_len: usize, comp: &[u8]) -> Result<()> {
        for (i, chunk) in comp.chunks(CHUNK_BYTES).enumerate() {
            let off = (i * CHUNK_BYTES) as u32;
            let mut payload = u32_words(&[addr, raw_len as u32, comp.len() as u32, off, chunk.len() as u32]);
            payload.extend(bytes_to_words(chunk));
            self.send(LOAD | LZ4_FLAG, &payload).await?;
            self.expect_ack(|| format!("LZ4 LOAD at offset {off}"), BULK).await?;
        }
        Ok(())
    }

    /// Load `data` at `addr`: LZ4 when asked for, the monitor has it, and it
    /// shrinks the data enough; plain LOAD otherwise.
    pub async fn load(&mut self, addr: u32, data: &[u8], opts: &LoadOptions) -> Result<LoadStats> {
        if opts.lz4 && self.caps & CAP_LZ4 != 0 && !data.is_empty() {
            let comp = lz4::compress(data, opts.max_match);
            if (comp.len() as f64) < data.len() as f64 * opts.max_ratio {
                self.load_lz4(addr, data.len(), &comp).await?;
                return Ok(LoadStats {
                    bytes: data.len(),
                    lz4_bytes: Some(comp.len()),
                });
            }
        }
        self.load_raw(addr, data).await?;
        Ok(LoadStats {
            bytes: data.len(),
            lz4_bytes: None,
        })
    }

    pub async fn read_mem(&mut self, addr: u32, len: u32) -> Result<Vec<u8>> {
        self.send(READ_MEM, &u32_words(&[addr, len])).await?;
        let mut out = Vec::with_capacity(len as usize);
        loop {
            let what = || format!("READ_MEM at 0x{addr:08x}");
            let f = match self.wait_frame(&[DATA, ERROR], Instant::now() + BULK).await? {
                None => return Err(SessionError::Timeout(what())),
                Some(f) if f.ty == ERROR => {
                    return Err(SessionError::Monitor {
                        what: what(),
                        code: f.words.first().copied().unwrap_or(0),
                    });
                }
                Some(f) if !f.ok => return Err(SessionError::Checksum(what())),
                Some(f) => f,
            };
            let n = word_u32(&f.words, 0) as usize;
            out.extend(words_to_bytes(&f.words, 2, n));
            // len 0 is answered by one empty DATA frame.
            if out.len() >= len as usize {
                return Ok(out);
            }
        }
    }

    pub async fn get_regs(&mut self) -> Result<[u32; NUM_REGS]> {
        self.send(GET_REGS, &[]).await?;
        match self.wait_frame(&[REGS, ERROR], Instant::now() + SHORT).await? {
            Some(f) if f.ty == REGS && f.ok => Ok(std::array::from_fn(|i| word_u32(&f.words, 2 * i))),
            Some(f) if f.ty == ERROR => Err(SessionError::Monitor {
                what: "GET_REGS".into(),
                code: f.words.first().copied().unwrap_or(0),
            }),
            Some(_) => Err(SessionError::Checksum("GET_REGS".into())),
            None => Err(SessionError::Timeout("GET_REGS".into())),
        }
    }

    pub async fn set_reg(&mut self, index: u16, value: u32) -> Result<()> {
        let mut payload = vec![index];
        payload.extend(u32_words(&[value]));
        self.send(SET_REG, &payload).await?;
        self.expect_ack(|| format!("SET_REG {index}"), SHORT).await
    }

    pub async fn cont(&mut self) -> Result<()> {
        self.send(CONT, &[]).await?;
        self.expect_ack(|| "CONT".into(), SHORT).await
    }

    pub async fn run(&mut self, pc: u32, gp: u32, sp: u32) -> Result<()> {
        self.send(RUN, &u32_words(&[pc, gp, sp])).await?;
        self.expect_ack(|| "RUN".into(), SHORT).await
    }

    /// SET_BAUD to `reload`, then PING at the new rate and confirm with
    /// PING [1]. If the new rate does not answer, go back to the old one,
    /// let the monitor's windows close, and PING there. Returns the rate the
    /// link ends up at.
    pub async fn negotiate_rate(&mut self, reload: u16) -> Result<u32> {
        let old = self.io.baud_rate();
        self.send(SET_BAUD, &[reload]).await?;
        match self
            .wait_frame(&[ACK, ERROR], Instant::now() + Duration::from_secs(1))
            .await?
        {
            Some(f) if f.ty == ACK => {}
            _ => return Ok(old),
        }
        let new = proto::sio1_rate(reload);
        sleep(Duration::from_millis(20)).await;
        self.io.set_baud_rate(new)?;
        if self.ping(RATE_TRY, &[]).await? && self.ping(RATE_TRY, &[1]).await? {
            self.take_text();
            return Ok(new);
        }
        self.io.set_baud_rate(old)?;
        sleep(RATE_WINDOW).await;
        self.take_text();
        if !self.ping(Duration::from_secs(3), &[]).await? {
            return Err(SessionError::Other(format!("lost after trying reload {reload}")));
        }
        // Whatever a garbled PONG decoded to is not console text.
        self.take_text();
        Ok(old)
    }

    /// Wait for the running program to stop, until `deadline`, serving its
    /// PCDRV calls. `break 4, 0` is an exit with the code in a0; `break 0,
    /// 0x101..0x107` is a PCDRV call, served and continued. Without a PCDRV
    /// server every call fails with -1, so a program probing PCinit sees no
    /// host rather than hanging. Any other stop is returned as is, except
    /// the legacy EXIT reason, whose code is in `a`.
    pub async fn run_until_stop(
        &mut self,
        deadline: Instant,
        mut pcdrv: Option<&mut PcdrvServer>,
    ) -> Result<RunResult> {
        loop {
            let slot = deadline.min(Instant::now() + Duration::from_millis(50));
            if let Some(f) = self.wait_frame(&[STOPPED], slot).await? {
                let stop = Stop {
                    reason: f.words.first().copied().unwrap_or(0),
                    epc: word_u32(&f.words, 1),
                    a: word_u32(&f.words, 3),
                    b: word_u32(&f.words, 5),
                };
                let insn = if stop.reason == STOP_BREAKPOINT { stop.a } else { 0 };
                if let Some(code) = BreakCode::decode(insn) {
                    if code.is_exit() {
                        let regs = self.get_regs().await?;
                        let a0 = regs[REG_A0 as usize];
                        return Ok(RunResult {
                            stop: Some(Stop {
                                reason: STOP_EXIT,
                                a: a0,
                                ..stop
                            }),
                            exit_code: Some(a0),
                        });
                    }
                    if let Some(op) = code.pcdrv_op() {
                        self.serve_pcdrv(op, stop.epc, pcdrv.as_deref_mut()).await?;
                        continue;
                    }
                }
                // Monitors before break-driven PCDRV report exit themselves, code in a.
                let exit_code = (stop.reason == STOP_EXIT).then_some(stop.a);
                return Ok(RunResult {
                    stop: Some(stop),
                    exit_code,
                });
            }
            if Instant::now() >= deadline {
                return Ok(RunResult {
                    stop: None,
                    exit_code: None,
                });
            }
        }
    }

    /// Serve a PCDRV call the target made with `break 0, op`: arguments from
    /// its registers and memory, the result back in v0/v1 (v0 alone for init
    /// and close), then resume past the break.
    async fn serve_pcdrv(&mut self, op: u32, epc: u32, pcdrv: Option<&mut PcdrvServer>) -> Result<()> {
        let r = self.get_regs().await?;
        let a = |i: u16| r[(REG_A0 + i) as usize];
        let (a0, a1, a2, a3) = (a(0), a(1), a(2), a(3));
        let ret = match pcdrv {
            None => -1,
            Some(server) => match self.pcdrv_call(server, op, a0, a1, a2, a3).await {
                Ok(v) => v,
                Err(e) => {
                    if self.verbose {
                        eprintln!("psxmon: pcdrv 0x{op:03x} failed: {e}");
                    }
                    -1
                }
            },
        };
        if self.verbose {
            eprintln!("psxmon: pcdrv 0x{op:03x} a0={a0:#x} a1={a1:#x} a2={a2:#x} a3={a3:#x} -> {ret}");
        }
        if op == PC_INIT || op == PC_CLOSE {
            self.set_reg(REG_V0, ret as u32).await?;
        } else {
            self.set_reg(REG_V0, 0).await?;
            self.set_reg(REG_V1, ret as u32).await?;
        }
        self.set_reg(REG_PC, epc.wrapping_add(4)).await?;
        self.cont().await
    }

    async fn pcdrv_call(
        &mut self,
        s: &mut PcdrvServer,
        op: u32,
        a0: u32,
        a1: u32,
        a2: u32,
        a3: u32,
    ) -> std::result::Result<i32, CallError> {
        Ok(match op {
            PC_INIT => 0,
            PC_CREAT | PC_OPEN => {
                let raw = self.read_mem(a0, PCDRV_NAME_MAX).await?;
                let name = raw.split(|&b| b == 0).next().unwrap_or(&[]);
                if op == PC_CREAT {
                    s.create(name)?
                } else {
                    s.open(name, a2)?
                }
            }
            PC_CLOSE => s.close(a0 as i32),
            PC_READ => {
                let data = s.read(a1 as i32, a2)?;
                if !data.is_empty() {
                    self.write_mem(a3, &data).await?;
                }
                data.len() as i32
            }
            PC_WRITE => {
                let len = a2 as i32;
                if len < 0 {
                    -1
                } else {
                    s.check_write(a1 as i32, len as u64)?;
                    let data = if len > 0 {
                        self.read_mem(a3, len as u32).await?
                    } else {
                        Vec::new()
                    };
                    s.write(a1 as i32, &data)?
                }
            }
            PC_LSEEK => s.seek(a0 as i32, a2 as i32, a3)?,
            _ => -1,
        })
    }
}

/// The two ways a PCDRV call can fail; either becomes -1 for the target.
#[derive(Debug, thiserror::Error)]
enum CallError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Pcdrv(#[from] crate::pcdrv::PcdrvError),
}
