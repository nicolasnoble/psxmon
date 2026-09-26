//! A fake monitor for tests: speaks the SIO1 byte-stream protocol the way
//! monitor.c and transport.c do, over the in-memory transport, with 2 MiB of
//! RAM, a register file, SET_BAUD with its two windows, and a scripted
//! target program that prints, makes PCDRV breaks and exits.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use psxmon::frame::{bytes_to_words, fletcher, u32_words, word_u32, words_to_bytes};
use psxmon::lz4;
use psxmon::proto::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::time::{Instant, timeout_at};

pub const RAM_SIZE: usize = 2 << 20;
const RUN_SR: u32 = 0x4000_0404;

pub struct Machine {
    pub ram: Vec<u8>,
    pub regs: [u32; NUM_REGS],
}

impl Machine {
    fn phys(addr: u32) -> Option<usize> {
        let p = (addr & 0x1fff_ffff) as usize;
        (p < RAM_SIZE).then_some(p)
    }
    pub fn read8(&self, addr: u32) -> u8 {
        Self::phys(addr).map_or(0, |p| self.ram[p])
    }
    pub fn write8(&mut self, addr: u32, v: u8) {
        if let Some(p) = Self::phys(addr) {
            self.ram[p] = v;
        }
    }
    pub fn read(&self, addr: u32, len: usize) -> Vec<u8> {
        (0..len).map(|i| self.read8(addr.wrapping_add(i as u32))).collect()
    }
    pub fn write(&mut self, addr: u32, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            self.write8(addr.wrapping_add(i as u32), b);
        }
    }
}

/// What the target does next.
pub enum Step {
    /// Console text through the tty device (0x00 bytes are dropped).
    Tty(Vec<u8>),
    /// A software `break` at the address in regs[37].
    Break(u32),
    /// Spin forever.
    Hang,
}

pub trait Program: Send {
    /// Called on RUN (fresh start, regs as RUN left them) and on every
    /// resume past a break.
    fn step(&mut self, m: &mut Machine) -> Step;
}

#[derive(Clone)]
pub struct SimConfig {
    pub caps: u16,
    pub bios: u32,
    /// Report `break 4, 0` as STOPPED EXIT with the code in `a`, as monitors
    /// before break-driven exit did.
    pub legacy_exit: bool,
    /// SET_BAUD window length.
    pub window: Duration,
    /// Switch to a rate the host can never match (to exercise the fallback).
    pub unreachable_rate: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            caps: CAP_LZ4,
            bios: 0xbf38df5e,
            legacy_exit: false,
            window: Duration::from_millis(400),
            unreachable_rate: false,
        }
    }
}

/// What the sim saw, for assertions.
#[derive(Default, Debug, Clone)]
pub struct SimStats {
    pub frames: usize,
    pub lz4_frames: usize,
    pub plain_load_frames: usize,
    pub max_match: usize,
    pub errors_sent: Vec<u16>,
    pub rate: u32,
    pub stops: usize,
}

#[derive(Default)]
struct Lz4State {
    active: bool,
    dest: u32,
    consumed: u32,
    comp: Vec<u8>,
}

pub struct Sim {
    io: DuplexStream,
    rx: VecDeque<u8>,
    rate: u32,
    host_rate: Arc<AtomicU32>,
    cfg: SimConfig,
    pub m: Machine,
    ctx: bool,
    epc: u32,
    last_break: u32,
    program: Box<dyn Program>,
    lz: Lz4State,
    stats: Arc<Mutex<SimStats>>,
}

impl Sim {
    pub fn new(
        io: DuplexStream,
        host_rate: Arc<AtomicU32>,
        cfg: SimConfig,
        program: Box<dyn Program>,
    ) -> (Sim, Arc<Mutex<SimStats>>) {
        let rate = host_rate.load(Ordering::SeqCst);
        let stats = Arc::new(Mutex::new(SimStats {
            rate,
            ..Default::default()
        }));
        (
            Sim {
                io,
                rx: VecDeque::new(),
                rate,
                host_rate,
                cfg,
                m: Machine {
                    ram: vec![0; RAM_SIZE],
                    regs: [0; NUM_REGS],
                },
                ctx: false,
                epc: 0,
                last_break: 0,
                program,
                lz: Lz4State::default(),
                stats: stats.clone(),
            },
            stats,
        )
    }

    fn in_sync(&self) -> bool {
        self.host_rate.load(Ordering::SeqCst) == self.rate
    }

    /// Next byte from the host, or None at `deadline` / end of link. Bytes
    /// sent at a rate the sim is not on are lost.
    async fn byte_until(&mut self, deadline: Option<Instant>) -> Option<u8> {
        loop {
            if let Some(b) = self.rx.pop_front() {
                if self.in_sync() {
                    return Some(b);
                }
                continue;
            }
            let mut buf = [0u8; 4096];
            let n = match deadline {
                Some(d) => match timeout_at(d, self.io.read(&mut buf)).await {
                    Ok(r) => r.ok()?,
                    Err(_) => return None,
                },
                None => self.io.read(&mut buf).await.ok()?,
            };
            if n == 0 {
                return None;
            }
            self.rx.extend(&buf[..n]);
        }
    }

    async fn byte(&mut self) -> Option<u8> {
        self.byte_until(None).await
    }

    async fn word(&mut self) -> Option<u16> {
        let lo = self.byte().await? as u16;
        let hi = self.byte().await? as u16;
        Some(lo | hi << 8)
    }

    async fn put(&mut self, bytes: &[u8]) {
        if self.in_sync() {
            let _ = self.io.write_all(bytes).await;
        }
    }

    async fn send_frame(&mut self, ty: u16, payload: &[u16]) {
        let wire = psxmon::frame::encode_frame(ty, payload);
        self.put(&wire).await;
    }

    async fn send_status(&mut self, code: u16) {
        if code == 0 {
            self.send_frame(ACK, &[]).await;
        } else {
            self.stats.lock().unwrap().errors_sent.push(code);
            self.send_frame(ERROR, &[code]).await;
        }
    }

    /// transportRecvBegin + the payload + CKSUM. None at end of link.
    async fn recv_frame(&mut self) -> Option<(u16, Vec<u16>, bool)> {
        let (ty, len) = loop {
            if self.byte().await? != 0 {
                continue;
            }
            let mut lo;
            loop {
                lo = self.byte().await?;
                if lo != 0 {
                    break;
                }
            }
            let hi = self.byte().await?;
            if (lo as u16 | (hi as u16) << 8) != SYNC {
                continue;
            }
            let t = self.word().await?;
            let l = self.word().await?;
            if l <= STREAM_MAX_LEN {
                break (t, l);
            }
        };
        let mut words = Vec::with_capacity(len as usize);
        for _ in 0..len {
            words.push(self.word().await?);
        }
        let lo = self.word().await? as u32;
        let hi = self.word().await? as u32;
        let ck = lo | hi << 16;
        // Mandatory on a byte link: 0 never matches.
        let ok = ck == fletcher([ty, len].into_iter().chain(words.iter().copied()));
        self.stats.lock().unwrap().frames += 1;
        Some((ty, words, ok))
    }

    /// awaitFrame: the exact bytes of `f` within one window.
    async fn await_exact(&mut self, f: &[u8]) -> bool {
        let deadline = Instant::now() + self.cfg.window;
        let mut matched = 0;
        while let Some(b) = self.byte_until(Some(deadline)).await {
            if b == f[matched] {
                matched += 1;
                if matched == f.len() {
                    return true;
                }
            } else {
                matched = if b == f[0] { 1 } else { 0 };
            }
        }
        false
    }

    pub async fn run(mut self) {
        // The monitor waits in the command loop from boot (HELLO is lost on SIO1).
        while let Some((ty, words, ok)) = self.recv_frame().await {
            if !self.command(ty, &words, ok).await {
                // A hung target: the link stays open and silent.
                std::future::pending::<()>().await;
            }
        }
    }

    /// One command; false when the target hangs for good.
    async fn command(&mut self, ty: u16, w: &[u16], ok: bool) -> bool {
        let base = ty & !LZ4_FLAG;
        if base == WRITE_MEM || base == LOAD {
            let reply = if ty & LZ4_FLAG == 0 {
                self.stats.lock().unwrap().plain_load_frames += 1;
                let addr = word_u32(w, 0);
                let n = word_u32(w, 2) as usize;
                let avail = w.len().saturating_sub(4) * 2;
                let bytes = words_to_bytes(w, 4, n.min(avail));
                self.m.write(addr, &bytes); // before the checksum is known
                if ok { 0 } else { E_CKSUM }
            } else if self.cfg.caps & CAP_LZ4 != 0 {
                self.lz4_slice(w, ok)
            } else {
                E_BADCMD
            };
            self.send_status(reply).await;
            return true;
        }
        if !ok {
            self.send_status(E_CKSUM).await;
            return true;
        }
        if w.len() > CMD_MAX_WORDS {
            self.send_status(E_BADLEN).await;
            return true;
        }
        match ty {
            PING => {
                let mut p = vec![PROTO_VER, self.cfg.caps];
                p.extend(u32_words(&[self.cfg.bios]));
                self.send_frame(PONG, &p).await;
            }
            READ_MEM => {
                let (addr, len) = (word_u32(w, 0), word_u32(w, 2) as usize);
                let mut off = 0;
                loop {
                    let chunk = (len - off).min(CHUNK_BYTES);
                    let mut p = u32_words(&[chunk as u32]);
                    p.extend(bytes_to_words(&self.m.read(addr.wrapping_add(off as u32), chunk)));
                    self.send_frame(DATA, &p).await;
                    off += chunk;
                    if off >= len {
                        break;
                    }
                }
            }
            GET_REGS => {
                let regs: Vec<u32> = (0..NUM_REGS)
                    .map(|i| if self.ctx || i == 35 { self.m.regs[i] } else { 0 })
                    .collect();
                self.send_frame(REGS, &u32_words(&regs)).await;
            }
            SET_REG => {
                let idx = w.first().copied().unwrap_or(0) as usize;
                let code = if idx > 37 {
                    E_BADREG
                } else if !self.ctx {
                    E_BADSTATE
                } else {
                    if idx != 0 {
                        self.m.regs[idx] = word_u32(w, 1);
                    }
                    0
                };
                self.send_status(code).await;
            }
            RUN => {
                let (pc, gp, sp) = (word_u32(w, 0), word_u32(w, 2), word_u32(w, 4));
                self.m.regs = [0; NUM_REGS];
                self.m.regs[28] = gp;
                self.m.regs[29] = sp;
                self.m.regs[30] = sp;
                self.m.regs[37] = pc;
                self.m.regs[REG_SR as usize] = RUN_SR;
                self.ctx = false;
                self.send_status(0).await;
                return self.execute(false).await;
            }
            CONT => {
                if !self.ctx {
                    self.send_status(E_BADSTATE).await;
                } else {
                    self.send_status(0).await;
                    // PC left on the break re-executes it.
                    let again = self.m.regs[37] == self.epc;
                    return self.execute(again).await;
                }
            }
            SET_BAUD => {
                let reload = w.first().copied().unwrap_or(0);
                if reload == 0 {
                    self.send_status(E_BADLEN).await;
                } else {
                    self.send_status(0).await;
                    self.try_rate(reload).await;
                }
            }
            STOP => self.send_status(0).await,
            _ => self.send_status(E_BADCMD).await,
        }
        true
    }

    fn lz4_slice(&mut self, w: &[u16], ok: bool) -> u16 {
        self.stats.lock().unwrap().lz4_frames += 1;
        if w.len() < 10 {
            self.lz.active = false;
            return E_BADLEN;
        }
        let (dest, raw_len, clen, off, nbytes) = (
            word_u32(w, 0),
            word_u32(w, 2),
            word_u32(w, 4),
            word_u32(w, 6),
            word_u32(w, 8),
        );
        let mut bad = 0;
        if off == 0 {
            self.lz = Lz4State {
                active: true,
                dest,
                consumed: 0,
                comp: Vec::new(),
            };
        } else if !self.lz.active || off != self.lz.consumed {
            bad = E_BADSTATE;
        }
        if bad == 0 && (nbytes as usize > (w.len() - 10) * 2 || off + nbytes > clen) {
            bad = E_BADLEN;
        }
        if bad == 0 {
            self.lz.comp.extend(words_to_bytes(w, 10, nbytes as usize));
        }
        if bad == 0 && !ok {
            bad = E_CKSUM;
        }
        if bad == 0 {
            self.lz.consumed += nbytes;
            if self.lz.consumed == clen {
                let seqs = lz4::sequences(&self.lz.comp).unwrap_or_default();
                let mm = seqs.iter().map(|s| s.match_len).max().unwrap_or(0);
                {
                    let mut st = self.stats.lock().unwrap();
                    st.max_match = st.max_match.max(mm);
                }
                match lz4::decompress(&self.lz.comp, RAM_SIZE) {
                    Ok(out) if out.len() == raw_len as usize => {
                        let dest = self.lz.dest;
                        self.m.write(dest, &out);
                    }
                    _ => bad = E_DECODE,
                }
                self.lz.active = false;
            }
        }
        if bad != 0 {
            self.lz.active = false;
        }
        bad
    }

    async fn try_rate(&mut self, reload: u16) {
        const PING_BYTES: [u8; 11] = [0x00, 0xaa, 0x55, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00];
        const CONFIRM_BYTES: [u8; 13] = [
            0x00, 0xaa, 0x55, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x03, 0x00, 0x06, 0x00,
        ];
        let old = self.rate;
        self.rate = if self.cfg.unreachable_rate {
            1
        } else {
            sio1_rate(reload)
        };
        if self.await_exact(&PING_BYTES).await {
            self.send_frame(PONG, &[PROTO_VER]).await;
            if self.await_exact(&CONFIRM_BYTES).await {
                self.send_frame(PONG, &[PROTO_VER]).await;
                self.stats.lock().unwrap().rate = self.rate;
                return;
            }
        }
        self.rate = old;
    }

    /// Run the target until it stops (true) or hangs (false).
    async fn execute(&mut self, reexecute: bool) -> bool {
        let mut step = if reexecute {
            Step::Break(self.last_break)
        } else {
            self.program.step(&mut self.m)
        };
        loop {
            match step {
                Step::Tty(bytes) => {
                    let bytes: Vec<u8> = bytes.into_iter().filter(|&b| b != 0).collect();
                    self.put(&bytes).await;
                }
                Step::Hang => return false,
                Step::Break(insn) => {
                    self.ctx = true;
                    self.epc = self.m.regs[37];
                    self.last_break = insn;
                    self.m.regs[REG_BADVADDR as usize] = 0;
                    self.stats.lock().unwrap().stops += 1;
                    let (reason, a) = if self.cfg.legacy_exit && insn == 0x0004_000d {
                        (STOP_EXIT, self.m.regs[4])
                    } else {
                        (STOP_BREAKPOINT, insn)
                    };
                    self.send_frame(STOPPED, &[&[reason][..], &u32_words(&[self.epc, a, 0])].concat())
                        .await;
                    return true;
                }
            }
            step = self.program.step(&mut self.m);
        }
    }
}

/// Start a sim on a fresh in-memory link; returns the host end.
pub fn start(cfg: SimConfig, program: Box<dyn Program>) -> (psxmon::MemTransport, Arc<Mutex<SimStats>>) {
    let (host, dev, rate) = psxmon::MemTransport::pair(115200);
    let (sim, stats) = Sim::new(dev, rate, cfg, program);
    tokio::spawn(sim.run());
    (host, stats)
}

// ---- target programs ----

fn brk(m: &mut Machine, at: u32, code1: u32, code2: u32) -> Step {
    m.regs[37] = at;
    Step::Break(BreakCode { code1, code2 }.encode())
}

fn exit(m: &mut Machine, at: u32, code: u32) -> Step {
    m.regs[4] = code;
    brk(m, at, 4, 0)
}

/// PCDRV result the way the pcdrv.h wrappers read it: v1 when v0 is 0.
fn pcret(m: &Machine) -> i32 {
    if m.regs[2] == 0 { m.regs[3] as i32 } else { -1 }
}

/// Checks that RUN set pc/gp/sp and that `expect` is in memory at `addr`,
/// prints, then exits with `code` (0xdead if anything was off).
pub struct CheckAndExit {
    pub addr: u32,
    pub expect: Vec<u8>,
    pub pc: u32,
    pub gp: u32,
    pub sp: u32,
    pub code: u32,
    pub started: bool,
}

impl Program for CheckAndExit {
    fn step(&mut self, m: &mut Machine) -> Step {
        if !self.started {
            self.started = true;
            return Step::Tty(b"target: hello\n".to_vec());
        }
        let r = &m.regs;
        let good = m.read(self.addr, self.expect.len()) == self.expect
            && r[37] == self.pc
            && r[28] == self.gp
            && r[29] == self.sp
            && r[30] == self.sp
            && r[REG_SR as usize] == RUN_SR;
        let code = if good { self.code } else { 0xdead };
        exit(m, self.pc + 0x100, code)
    }
}

/// The farmjob test program, in Rust: read IN.TXT via PCDRV, write it back
/// upper-cased to OUT.TXT, exit with the byte count (0xbad on failure).
pub struct Farmjob {
    phase: u32,
    fd: i32,
    n: i32,
}

impl Farmjob {
    pub const NAME: u32 = 0x800f_0000;
    pub const BUF: u32 = 0x8010_0000;
    pub const BUF_SIZE: u32 = 32768;

    pub fn new() -> Self {
        Farmjob { phase: 0, fd: -1, n: 0 }
    }
}

impl Program for Farmjob {
    fn step(&mut self, m: &mut Machine) -> Step {
        let at = 0x8001_0100 + 8 * self.phase;
        self.phase += 1;
        let bad = |m: &mut Machine| exit(m, 0x8001_0ff0, 0xbad);
        match self.phase - 1 {
            0 => Step::Tty(b"farmjob: start\n".to_vec()),
            1 => brk(m, at, 0, PC_INIT),
            2 => {
                if m.regs[2] != 0 {
                    return bad(m);
                }
                m.write(Self::NAME, b"IN.TXT\0");
                m.regs[4] = Self::NAME;
                m.regs[6] = 0;
                brk(m, at, 0, PC_OPEN)
            }
            3 => {
                self.fd = pcret(m);
                if self.fd < 0 {
                    return bad(m);
                }
                m.regs[5] = self.fd as u32;
                m.regs[6] = Self::BUF_SIZE;
                m.regs[7] = Self::BUF;
                brk(m, at, 0, PC_READ)
            }
            4 => {
                self.n = pcret(m);
                m.regs[4] = self.fd as u32;
                brk(m, at, 0, PC_CLOSE)
            }
            5 => {
                if m.regs[2] != 0 || self.n < 0 {
                    return bad(m);
                }
                let data = m.read(Self::BUF, self.n as usize).to_ascii_uppercase();
                m.write(Self::BUF, &data);
                m.write(Self::NAME, b"OUT.TXT\0");
                m.regs[4] = Self::NAME;
                m.regs[6] = 0;
                brk(m, at, 0, PC_CREAT)
            }
            6 => {
                self.fd = pcret(m);
                if self.fd < 0 {
                    return bad(m);
                }
                m.regs[5] = self.fd as u32;
                m.regs[6] = self.n as u32;
                m.regs[7] = Self::BUF;
                brk(m, at, 0, PC_WRITE)
            }
            7 => {
                if pcret(m) != self.n {
                    return bad(m);
                }
                m.regs[4] = self.fd as u32;
                brk(m, at, 0, PC_CLOSE)
            }
            8 => {
                if m.regs[2] != 0 {
                    return bad(m);
                }
                Step::Tty(format!("farmjob: {} bytes\n", self.n).into_bytes())
            }
            _ => exit(m, at, self.n as u32),
        }
    }
}

/// Tries to PCcreat a name outside the jail, then exits with the result.
pub struct JailProbe {
    pub name: &'static [u8],
    pub phase: u32,
}

impl Program for JailProbe {
    fn step(&mut self, m: &mut Machine) -> Step {
        self.phase += 1;
        match self.phase {
            1 => {
                let mut name = self.name.to_vec();
                name.push(0);
                m.write(Farmjob::NAME, &name);
                m.regs[4] = Farmjob::NAME;
                brk(m, 0x8001_0000, 0, PC_CREAT)
            }
            _ => {
                let r = pcret(m) as u32;
                exit(m, 0x8001_0010, r)
            }
        }
    }
}

pub struct Hang;

impl Program for Hang {
    fn step(&mut self, _m: &mut Machine) -> Step {
        Step::Hang
    }
}
