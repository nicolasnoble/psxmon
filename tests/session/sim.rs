//! A fake monitor for tests: speaks the SIO1 byte-stream protocol the way
//! monitor.c and transport.c do, over the in-memory transport, with 2 MiB of
//! RAM, a register file, SET_BAUD with its two windows, and a scripted
//! target program that prints, makes PCDRV breaks and exits.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use psxmon::frame::{bytes_to_words, encode_frame, fletcher, u32_words, word_u32, words_to_bytes};
use psxmon::lz4;
use psxmon::proto::*;
use psxmon::session::deadline_after;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::time::{Instant, timeout_at};

pub const RAM_SIZE: usize = 2 << 20;
pub const RUN_SR: u32 = 0x4000_0404;
const EXIT_BREAK: u32 = 0x0004_000d;

// Register indices, as in REGS.
pub const V0: u16 = 2;
pub const V1: u16 = 3;
pub const A0: u16 = 4;
pub const A1: u16 = 5;
pub const A2: u16 = 6;
pub const A3: u16 = 7;
pub const GP: u16 = 28;
pub const SP: u16 = 29;
pub const FP: u16 = 30;

pub struct Machine {
    pub ram: Vec<u8>,
    pub regs: [u32; NUM_REGS],
}

impl Machine {
    fn phys(addr: u32) -> Option<usize> {
        usize::try_from(addr & 0x1fff_ffff)
            .ok()
            .filter(|&p| p < RAM_SIZE)
    }

    // Byte addresses advance in the 32-bit address space, which wraps.
    pub fn read(&self, addr: u32, len: usize) -> Vec<u8> {
        let mut at = addr;
        (0..len)
            .map(|_| {
                let b = Self::phys(at)
                    .and_then(|p| self.ram.get(p))
                    .copied()
                    .unwrap_or(0);
                at = at.wrapping_add(1);
                b
            })
            .collect()
    }

    pub fn write(&mut self, addr: u32, data: &[u8]) {
        let mut at = addr;
        for &b in data {
            if let Some(slot) = Self::phys(at).and_then(|p| self.ram.get_mut(p)) {
                *slot = b;
            }
            at = at.wrapping_add(1);
        }
    }

    pub fn reg(&self, index: u16) -> u32 {
        self.regs.get(usize::from(index)).copied().unwrap_or(0)
    }

    pub fn set(&mut self, index: u16, value: u32) {
        if let Some(r) = self.regs.get_mut(usize::from(index)) {
            *r = value;
        }
    }
}

/// What the target does next.
pub enum Step {
    /// Console text through the tty device (0x00 bytes are dropped).
    Tty(Vec<u8>),
    /// A software `break` at the address in the PC register.
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
    /// The rate the sim listens at from the start (default: the host's).
    pub start_rate: Option<u32>,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            caps: CAP_LZ4,
            bios: 0xbf38df5e,
            legacy_exit: false,
            window: Duration::from_millis(400),
            unreachable_rate: false,
            start_rate: None,
        }
    }
}

/// What the sim saw, for assertions.
#[derive(Default, Debug, Clone)]
pub struct SimStats {
    pub lz4_frames: usize,
    pub plain_load_frames: usize,
    pub max_match: usize,
    pub errors_sent: Vec<u16>,
    pub rate: u32,
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
    m: Machine,
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
        let rate = cfg
            .start_rate
            .unwrap_or_else(|| host_rate.load(Ordering::SeqCst));
        let stats = Arc::new(Mutex::new(SimStats {
            rate,
            ..Default::default()
        }));
        let sim = Sim {
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
        };
        (sim, stats)
    }

    fn stats(&self) -> std::sync::MutexGuard<'_, SimStats> {
        self.stats.lock().expect("sim stats lock")
    }

    fn count(&self, field: impl FnOnce(&mut SimStats) -> &mut usize) {
        let mut st = self.stats();
        let n = field(&mut st);
        *n = n.saturating_add(1);
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
                Some(d) => timeout_at(d, self.io.read(&mut buf)).await.ok()?.ok()?,
                None => self.io.read(&mut buf).await.ok()?,
            };
            if n == 0 {
                return None;
            }
            self.rx.extend(buf.get(..n)?);
        }
    }

    async fn byte(&mut self) -> Option<u8> {
        self.byte_until(None).await
    }

    async fn word(&mut self) -> Option<u16> {
        let lo = self.byte().await?;
        let hi = self.byte().await?;
        Some(u16::from_le_bytes([lo, hi]))
    }

    async fn put(&mut self, bytes: &[u8]) {
        if self.in_sync() {
            // A closed link just ends the test's conversation.
            self.io.write_all(bytes).await.ok();
        }
    }

    async fn send_frame(&mut self, ty: u16, payload: &[u16]) {
        let wire = encode_frame(ty, payload).expect("sim frames fit");
        self.put(&wire).await;
    }

    async fn send_status(&mut self, code: u16) {
        if code == 0 {
            self.send_frame(ACK, &[]).await;
        } else {
            self.stats().errors_sent.push(code);
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
            if u16::from_le_bytes([lo, hi]) != SYNC {
                continue;
            }
            let t = self.word().await?;
            let l = self.word().await?;
            if l <= STREAM_MAX_LEN {
                break (t, l);
            }
        };
        let mut words = Vec::with_capacity(usize::from(len));
        for _ in 0..len {
            words.push(self.word().await?);
        }
        let lo = self.word().await?;
        let hi = self.word().await?;
        let ck = u32::from(lo) | (u32::from(hi) << 16);
        // Mandatory on a byte link: 0 never matches.
        let ok = ck == fletcher([ty, len].into_iter().chain(words.iter().copied()));
        Some((ty, words, ok))
    }

    /// awaitFrame: the exact bytes of `f` within one window.
    async fn await_exact(&mut self, f: &[u8]) -> bool {
        let deadline = deadline_after(self.cfg.window);
        let mut matched = 0usize;
        while let Some(b) = self.byte_until(Some(deadline)).await {
            if f.get(matched) == Some(&b) {
                matched = matched.saturating_add(1);
                if matched == f.len() {
                    return true;
                }
            } else {
                matched = usize::from(f.first() == Some(&b));
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
                self.count(|st| &mut st.plain_load_frames);
                let addr = word_u32(w, 0);
                let n = usize::try_from(word_u32(w, 2)).expect("len fits usize");
                let avail = w.len().saturating_sub(4).saturating_mul(2);
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
            READ_MEM => self.read_mem(word_u32(w, 0), word_u32(w, 2)).await,
            GET_REGS => {
                let regs: Vec<u32> = (0..NUM_REGS)
                    .map(|i| {
                        if self.ctx || i == usize::from(REG_BADVADDR) {
                            self.regs_at(i)
                        } else {
                            0
                        }
                    })
                    .collect();
                self.send_frame(REGS, &u32_words(&regs)).await;
            }
            SET_REG => {
                let idx = w.first().copied().unwrap_or(0);
                let code = if usize::from(idx) >= NUM_REGS {
                    E_BADREG
                } else if !self.ctx {
                    E_BADSTATE
                } else {
                    if idx != 0 {
                        self.m.set(idx, word_u32(w, 1));
                    }
                    0
                };
                self.send_status(code).await;
            }
            RUN => {
                self.m.regs = [0; NUM_REGS];
                self.m.set(GP, word_u32(w, 2));
                self.m.set(SP, word_u32(w, 4));
                self.m.set(FP, word_u32(w, 4));
                self.m.set(REG_PC, word_u32(w, 0));
                self.m.set(REG_SR, RUN_SR);
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
                    let again = self.m.reg(REG_PC) == self.epc;
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

    fn regs_at(&self, i: usize) -> u32 {
        self.m.regs.get(i).copied().unwrap_or(0)
    }

    async fn read_mem(&mut self, addr: u32, len: u32) {
        let len = usize::try_from(len).expect("len fits usize");
        let mut off = 0usize;
        loop {
            let chunk = len.saturating_sub(off).min(CHUNK_BYTES);
            let at = addr.wrapping_add(u32::try_from(off).expect("offset fits u32"));
            let mut p = u32_words(&[u32::try_from(chunk).expect("chunk fits u32")]);
            p.extend(bytes_to_words(&self.m.read(at, chunk)));
            self.send_frame(DATA, &p).await;
            off = off.saturating_add(chunk);
            if off >= len {
                break;
            }
        }
    }

    fn lz4_slice(&mut self, w: &[u16], ok: bool) -> u16 {
        self.count(|st| &mut st.lz4_frames);
        if w.len() < 10 {
            self.lz.active = false;
            return E_BADLEN;
        }
        let [dest, raw_len, clen, off, nbytes] = [0, 2, 4, 6, 8].map(|i| word_u32(w, i));
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
        let room = w.len().saturating_sub(10).saturating_mul(2);
        let n = usize::try_from(nbytes).expect("nbytes fits usize");
        if bad == 0 && (n > room || off.checked_add(nbytes).is_none_or(|end| end > clen)) {
            bad = E_BADLEN;
        }
        if bad == 0 {
            self.lz.comp.extend(words_to_bytes(w, 10, n));
        }
        if bad == 0 && !ok {
            bad = E_CKSUM;
        }
        if bad == 0 {
            self.lz.consumed = self.lz.consumed.saturating_add(nbytes);
            if self.lz.consumed == clen {
                let seqs = lz4::sequences(&self.lz.comp).unwrap_or_default();
                let mm = seqs.iter().map(|s| s.match_len).max().unwrap_or(0);
                {
                    let mut st = self.stats();
                    st.max_match = st.max_match.max(mm);
                }
                let want = usize::try_from(raw_len).expect("rawlen fits usize");
                match lz4::decompress(&self.lz.comp, RAM_SIZE) {
                    Ok(out) if out.len() == want => {
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
        const PING_BYTES: [u8; 11] = [
            0x00, 0xaa, 0x55, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00,
        ];
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
                let rate = self.rate;
                self.stats().rate = rate;
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
                    self.epc = self.m.reg(REG_PC);
                    self.last_break = insn;
                    self.m.set(REG_BADVADDR, 0);
                    let (reason, a) = if self.cfg.legacy_exit && insn == EXIT_BREAK {
                        (STOP_EXIT, self.m.reg(A0))
                    } else {
                        (STOP_BREAKPOINT, insn)
                    };
                    let mut p = vec![reason];
                    p.extend(u32_words(&[self.epc, a, 0]));
                    self.send_frame(STOPPED, &p).await;
                    return true;
                }
            }
            step = self.program.step(&mut self.m);
        }
    }
}

/// Start a sim on a fresh in-memory link; returns the host end, at 115200.
pub fn start(
    cfg: SimConfig,
    program: Box<dyn Program>,
) -> (psxmon::MemTransport, Arc<Mutex<SimStats>>) {
    let (host, dev, rate) = psxmon::MemTransport::pair(115200);
    let (sim, stats) = Sim::new(dev, rate, cfg, program);
    tokio::spawn(sim.run());
    (host, stats)
}

// ---- target programs ----

fn brk(m: &mut Machine, at: u32, code1: u32, code2: u32) -> Step {
    m.set(REG_PC, at);
    Step::Break(BreakCode { code1, code2 }.encode())
}

fn exit(m: &mut Machine, at: u32, code: u32) -> Step {
    m.set(A0, code);
    brk(m, at, 4, 0)
}

/// PCDRV result the way the pcdrv.h wrappers read it: v1 when v0 is 0.
fn pcret(m: &Machine) -> i32 {
    if m.reg(V0) == 0 {
        m.reg(V1).cast_signed()
    } else {
        -1
    }
}

/// Checks that RUN set pc/gp/sp and that `expect` is in memory at `addr`,
/// prints, then exits with `code` (0xdead if anything was off) from a break
/// at `exit_at`.
pub struct CheckAndExit {
    pub addr: u32,
    pub expect: Vec<u8>,
    pub pc: u32,
    pub gp: u32,
    pub sp: u32,
    pub code: u32,
    pub exit_at: u32,
    pub started: bool,
}

impl Program for CheckAndExit {
    fn step(&mut self, m: &mut Machine) -> Step {
        if !self.started {
            self.started = true;
            return Step::Tty(b"target: hello\n".to_vec());
        }
        let good = m.read(self.addr, self.expect.len()) == self.expect
            && m.reg(REG_PC) == self.pc
            && m.reg(GP) == self.gp
            && m.reg(SP) == self.sp
            && m.reg(FP) == self.sp
            && m.reg(REG_SR) == RUN_SR;
        let code = if good { self.code } else { 0xdead };
        exit(m, self.exit_at, code)
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
        Farmjob {
            phase: 0,
            fd: -1,
            n: 0,
        }
    }

    fn bytes(&self) -> usize {
        usize::try_from(self.n).expect("byte count checked non-negative")
    }
}

impl Program for Farmjob {
    fn step(&mut self, m: &mut Machine) -> Step {
        let phase = self.phase;
        self.phase = phase.saturating_add(1);
        // A distinct fake PC per break.
        let at = phase
            .checked_mul(8)
            .and_then(|o| 0x8001_0100u32.checked_add(o))
            .expect("few phases");
        let bad = |m: &mut Machine| exit(m, 0x8001_0ff0, 0xbad);
        match phase {
            0 => Step::Tty(b"farmjob: start\n".to_vec()),
            1 => brk(m, at, 0, PC_INIT),
            2 => {
                if m.reg(V0) != 0 {
                    return bad(m);
                }
                m.write(Self::NAME, b"IN.TXT\0");
                m.set(A0, Self::NAME);
                m.set(A2, 0);
                brk(m, at, 0, PC_OPEN)
            }
            3 => {
                self.fd = pcret(m);
                if self.fd < 0 {
                    return bad(m);
                }
                m.set(A1, self.fd.cast_unsigned());
                m.set(A2, Self::BUF_SIZE);
                m.set(A3, Self::BUF);
                brk(m, at, 0, PC_READ)
            }
            4 => {
                self.n = pcret(m);
                m.set(A0, self.fd.cast_unsigned());
                brk(m, at, 0, PC_CLOSE)
            }
            5 => {
                if m.reg(V0) != 0 || self.n < 0 {
                    return bad(m);
                }
                let data = m.read(Self::BUF, self.bytes()).to_ascii_uppercase();
                m.write(Self::BUF, &data);
                m.write(Self::NAME, b"OUT.TXT\0");
                m.set(A0, Self::NAME);
                m.set(A2, 0);
                brk(m, at, 0, PC_CREAT)
            }
            6 => {
                self.fd = pcret(m);
                if self.fd < 0 {
                    return bad(m);
                }
                m.set(A1, self.fd.cast_unsigned());
                m.set(A2, self.n.cast_unsigned());
                m.set(A3, Self::BUF);
                brk(m, at, 0, PC_WRITE)
            }
            7 => {
                if pcret(m) != self.n {
                    return bad(m);
                }
                m.set(A0, self.fd.cast_unsigned());
                brk(m, at, 0, PC_CLOSE)
            }
            8 => {
                if m.reg(V0) != 0 {
                    return bad(m);
                }
                Step::Tty(format!("farmjob: {} bytes\n", self.n).into_bytes())
            }
            _ => exit(m, at, self.n.cast_unsigned()),
        }
    }
}

/// Tries to PCcreat a name outside the jail, then exits with the result.
pub struct JailProbe {
    pub name: &'static [u8],
    pub done: bool,
}

impl Program for JailProbe {
    fn step(&mut self, m: &mut Machine) -> Step {
        if !self.done {
            self.done = true;
            let mut name = self.name.to_vec();
            name.push(0);
            m.write(Farmjob::NAME, &name);
            m.set(A0, Farmjob::NAME);
            return brk(m, 0x8001_0000, 0, PC_CREAT);
        }
        let r = pcret(m).cast_unsigned();
        exit(m, 0x8001_0010, r)
    }
}

pub struct Hang;

impl Program for Hang {
    fn step(&mut self, _m: &mut Machine) -> Step {
        Step::Hang
    }
}
