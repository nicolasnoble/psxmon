//! Session behaviour against the simulated monitor.
#![cfg(test)]

mod sim;

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use psxmon::exe;
use psxmon::pcdrv::{PcdrvServer, Quota};
use psxmon::proto::*;
use psxmon::session::{LoadOptions, Session, deadline_after};
use psxmon::{MemTransport, Transport};
use sim::*;
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

const BIOS: u32 = 0xbf38df5e;

async fn attach(
    cfg: SimConfig,
    program: Box<dyn Program>,
) -> (Session<MemTransport>, Arc<Mutex<SimStats>>) {
    let (host, stats) = sim::start(cfg, program);
    let mut s = Session::new(host);
    assert!(
        s.ping(Duration::from_secs(2), &[]).await.expect("ping"),
        "no PONG"
    );
    assert_eq!(s.version, Some(PROTO_VER));
    (s, stats)
}

fn lock(stats: &Arc<Mutex<SimStats>>) -> MutexGuard<'_, SimStats> {
    stats.lock().expect("sim stats lock")
}

fn len32(data: &[u8]) -> u32 {
    u32::try_from(data.len()).expect("test data fits u32")
}

/// Code-like bytes that compress but not trivially.
fn program_text(len: usize) -> Vec<u8> {
    let mut x: u32 = 7;
    (0..len)
        .map(|i| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let [_, _, _, noise] = x.to_le_bytes();
            let [low, ..] = (i % 16).to_le_bytes();
            if (i / 512) % 3 == 0 {
                0
            } else if (i / 64) % 2 == 0 {
                low
            } else {
                noise
            }
        })
        .collect()
}

async fn run_until_stop(
    s: &mut Session<MemTransport>,
    secs: u64,
    pcdrv: Option<&mut PcdrvServer>,
) -> psxmon::RunResult {
    s.run_until_stop(deadline_after(Duration::from_secs(secs)), pcdrv)
        .await
        .expect("run")
}

async fn load_run_exit(lz4: bool) {
    let text = program_text(100_000);
    let file = exe::build_ps_exe(&text, 0x8001_0000, 0x8001_0000, 0x8009_0000, 0x801f_0000)
        .expect("build PS-EXE");
    let img = exe::parse(&file).expect("parse PS-EXE");
    let seg = img.segments.first().expect("one segment").clone();
    let exit_at = img.pc.checked_add(0x100).expect("small");
    let prog = CheckAndExit {
        addr: seg.addr,
        expect: seg.data.clone(),
        pc: img.pc,
        gp: img.gp,
        sp: img.sp,
        code: 42,
        exit_at,
        started: false,
    };
    let (mut s, stats) = attach(SimConfig::default(), Box::new(prog)).await;
    assert_eq!(s.caps, CAP_LZ4);
    assert_eq!(s.bios, Some(BIOS));
    let opts = LoadOptions {
        lz4,
        ..Default::default()
    };
    let st = s.load(seg.addr, &seg.data, &opts).await.expect("load");
    assert_eq!(st.lz4_bytes.is_some(), lz4);
    s.run(img.pc, img.gp, img.sp).await.expect("RUN");
    let r = run_until_stop(&mut s, 5, None).await;
    assert_eq!(r.exit_code, Some(42));
    let stop = r.stop.expect("stopped");
    assert_eq!(stop.reason, STOP_EXIT);
    assert_eq!(stop.epc, exit_at);
    assert_eq!(s.take_text(), b"target: hello\n");
    let st = lock(&stats);
    if lz4 {
        assert!(st.lz4_frames >= 2, "lz4 frames {}", st.lz4_frames);
        assert_eq!(st.plain_load_frames, 0);
        assert!(
            st.max_match > 0 && st.max_match <= 128,
            "max match {}",
            st.max_match
        );
    } else {
        assert_eq!(st.lz4_frames, 0);
        assert_eq!(st.plain_load_frames, seg.data.len().div_ceil(8192));
    }
    assert!(st.errors_sent.is_empty(), "{:?}", st.errors_sent);
}

#[tokio::test]
async fn load_run_exit_plain() {
    load_run_exit(false).await;
}

#[tokio::test]
async fn load_run_exit_lz4() {
    load_run_exit(true).await;
}

#[tokio::test]
async fn lz4_falls_back_to_plain_without_cap() {
    let cfg = SimConfig {
        caps: 0,
        ..Default::default()
    };
    let (mut s, stats) = attach(cfg, Box::new(Hang)).await;
    assert_eq!(s.caps, 0);
    let st = s
        .load(0x8001_0000, &[0u8; 20000], &LoadOptions::default())
        .await
        .expect("load");
    assert_eq!(st.lz4_bytes, None);
    assert_eq!(lock(&stats).plain_load_frames, 3);
}

async fn farmjob(lz4: bool, size: usize) {
    let dir = tempfile::tempdir().expect("tempdir");
    let input: Vec<u8> = b"abcdefghijklmnopqrstuvwxyz\n"
        .iter()
        .copied()
        .cycle()
        .take(size)
        .collect();
    std::fs::write(dir.path().join("IN.TXT"), &input).expect("write IN.TXT");
    let (mut s, _stats) = attach(SimConfig::default(), Box::new(Farmjob::new())).await;
    // A stand-in body so the load path runs too.
    let body = program_text(30_000);
    let opts = LoadOptions {
        lz4,
        ..Default::default()
    };
    s.load(0x8001_0000, &body, &opts).await.expect("load");
    let mut server = PcdrvServer::new(dir.path(), Quota::default()).expect("PCDRV server");
    s.run(0x8001_0000, 0, 0x801f_fff0).await.expect("RUN");
    let r = run_until_stop(&mut s, 10, Some(&mut server)).await;
    assert_eq!(r.exit_code, Some(len32(&input)));
    let out = std::fs::read(dir.path().join("OUT.TXT")).expect("OUT.TXT written");
    assert_eq!(out, input.to_ascii_uppercase());
    let text = String::from_utf8(s.take_text()).expect("ASCII console text");
    assert_eq!(text, format!("farmjob: start\nfarmjob: {size} bytes\n"));
}

#[tokio::test]
async fn pcdrv_farmjob_20000_plain() {
    farmjob(false, 20000).await;
}

#[tokio::test]
async fn pcdrv_farmjob_20000_lz4() {
    farmjob(true, 20000).await;
}

#[tokio::test]
async fn pcdrv_farmjob_small_and_exact_chunk() {
    farmjob(true, 19).await;
    farmjob(true, 8192).await;
}

#[tokio::test]
async fn pcdrv_without_server_fails_calls() {
    let (mut s, _) = attach(SimConfig::default(), Box::new(Farmjob::new())).await;
    s.run(0x8001_0000, 0, 0x801f_fff0).await.expect("RUN");
    let r = run_until_stop(&mut s, 5, None).await;
    assert_eq!(r.exit_code, Some(0xbad));
}

#[tokio::test]
async fn pcdrv_jail_refuses_escape() {
    let outer = tempfile::tempdir().expect("tempdir");
    let base = outer.path().join("jail");
    std::fs::create_dir(&base).expect("jail dir");
    let probe = JailProbe {
        name: b"..\\escaped.txt",
        done: false,
    };
    let (mut s, _) = attach(SimConfig::default(), Box::new(probe)).await;
    let mut server = PcdrvServer::new(&base, Quota::default()).expect("PCDRV server");
    s.run(0x8001_0000, 0, 0x801f_fff0).await.expect("RUN");
    let r = run_until_stop(&mut s, 5, Some(&mut server)).await;
    assert_eq!(r.exit_code, Some(u32::MAX));
    assert!(!outer.path().join("escaped.txt").exists());
}

#[tokio::test]
async fn legacy_exit_reason_maps_to_exit_code() {
    let cfg = SimConfig {
        legacy_exit: true,
        ..Default::default()
    };
    let prog = CheckAndExit {
        addr: 0x8001_0000,
        expect: vec![],
        pc: 0x8001_0000,
        gp: 0,
        sp: 0x801f_fff0,
        code: 7,
        exit_at: 0x8001_0100,
        started: false,
    };
    let (mut s, _) = attach(cfg, Box::new(prog)).await;
    s.run(0x8001_0000, 0, 0x801f_fff0).await.expect("RUN");
    let r = run_until_stop(&mut s, 5, None).await;
    assert_eq!(r.exit_code, Some(7));
    let stop = r.stop.expect("stopped");
    assert_eq!(stop.reason, STOP_EXIT);
    assert_eq!(stop.a, 7);
}

#[tokio::test]
async fn set_baud_short_pong_keeps_caps_and_bios() {
    let (mut s, stats) = attach(SimConfig::default(), Box::new(Hang)).await;
    assert_eq!((s.caps, s.bios), (CAP_LZ4, Some(BIOS)));
    let rate = s.negotiate_rate(9).await.expect("SET_BAUD");
    assert_eq!(rate, 230400);
    assert_eq!(s.transport().baud_rate(), 230400);
    assert_eq!(lock(&stats).rate, 230400);
    // The two window PONGs carried only [proto_ver].
    assert_eq!((s.caps, s.bios), (CAP_LZ4, Some(BIOS)));
    assert!(s.ping(Duration::from_secs(1), &[]).await.expect("ping"));
    let data = program_text(20000);
    s.write_mem(0x8002_0000, &data).await.expect("WRITE_MEM");
    assert_eq!(
        s.read_mem(0x8002_0000, len32(&data))
            .await
            .expect("READ_MEM"),
        data
    );
}

#[tokio::test]
async fn set_baud_falls_back_when_new_rate_is_silent() {
    let cfg = SimConfig {
        unreachable_rate: true,
        ..Default::default()
    };
    let (mut s, stats) = attach(cfg, Box::new(Hang)).await;
    let rate = s.negotiate_rate(9).await.expect("SET_BAUD");
    assert_eq!(rate, 115200);
    assert_eq!(s.transport().baud_rate(), 115200);
    assert_eq!(lock(&stats).rate, 115200);
    assert_eq!((s.caps, s.bios), (CAP_LZ4, Some(BIOS)));
}

#[tokio::test]
async fn attach_finds_a_monitor_left_at_230400() {
    // An earlier run's SET_BAUD left the monitor at 230400; the host asks
    // for 115200 first.
    let cfg = SimConfig {
        start_rate: Some(230400),
        ..Default::default()
    };
    let (host, _stats) = sim::start(cfg, Box::new(Hang));
    let mut s = Session::new(host);
    let rates = [115200, 230400];
    let t0 = Instant::now();
    let found = s
        .attach_at(
            &rates,
            Duration::from_millis(600),
            Duration::from_millis(600),
        )
        .await
        .expect("attach");
    assert_eq!(found, Some(230400));
    assert!(
        t0.elapsed() >= Duration::from_millis(600),
        "115200 was tried first"
    );
    assert_eq!(s.transport().baud_rate(), 230400);
    assert_eq!((s.caps, s.bios), (CAP_LZ4, Some(BIOS)));
    // And the link works at that rate.
    let data = program_text(10000);
    s.write_mem(0x8002_0000, &data).await.expect("WRITE_MEM");
    assert_eq!(
        s.read_mem(0x8002_0000, len32(&data))
            .await
            .expect("READ_MEM"),
        data
    );
    // Nothing answers at rates the monitor is not on.
    let (host, _) = sim::start(
        SimConfig {
            start_rate: Some(57600),
            ..Default::default()
        },
        Box::new(Hang),
    );
    let mut s = Session::new(host);
    let none = s
        .attach_at(
            &rates,
            Duration::from_millis(300),
            Duration::from_millis(300),
        )
        .await;
    assert_eq!(none.expect("attach runs"), None);
}

#[tokio::test]
async fn memory_regs_and_errors() {
    let (mut s, stats) = attach(SimConfig::default(), Box::new(Hang)).await;
    // Across several DATA frames, an odd length, and len 0.
    let data = program_text(3 * 8192 + 5);
    s.write_mem(0x8003_0001, &data).await.expect("WRITE_MEM");
    assert_eq!(
        s.read_mem(0x8003_0001, len32(&data))
            .await
            .expect("READ_MEM"),
        data
    );
    assert_eq!(
        s.read_mem(0x8003_0001, 0).await.expect("READ_MEM 0"),
        Vec::<u8>::new()
    );
    // No halted context yet.
    assert!(
        s.get_regs()
            .await
            .expect("GET_REGS")
            .iter()
            .all(|&r| r == 0)
    );
    let e = s
        .set_reg(2, 1)
        .await
        .expect_err("SET_REG needs a halted context");
    assert!(e.to_string().contains("EBADSTATE"), "{e}");
    let e = s.cont().await.expect_err("CONT needs a halted context");
    assert!(e.to_string().contains("EBADSTATE"), "{e}");
    // A corrupted command frame is answered with ECKSUM.
    let mut bad = psxmon::frame::encode_frame(GET_REGS, &[]).expect("frame");
    if let Some(last) = bad.last_mut() {
        *last ^= 0x40;
    }
    s.transport().write_all(&bad).await.expect("write");
    let deadline = deadline_after(Duration::from_secs(1));
    let f = s
        .wait_frame(&[ERROR], deadline)
        .await
        .expect("wait")
        .expect("ERROR frame");
    assert_eq!(f.words, vec![E_CKSUM]);
    // STEP is reserved.
    s.send(STEP, &[]).await.expect("send");
    let deadline = deadline_after(Duration::from_secs(1));
    let f = s
        .wait_frame(&[ERROR], deadline)
        .await
        .expect("wait")
        .expect("ERROR frame");
    assert_eq!(f.words, vec![E_BADCMD]);
    assert_eq!(
        lock(&stats).errors_sent,
        vec![E_BADSTATE, E_BADSTATE, E_CKSUM, E_BADCMD]
    );
}

#[tokio::test]
async fn timeout_when_target_never_stops() {
    let (mut s, _) = attach(SimConfig::default(), Box::new(Hang)).await;
    s.run(0x8001_0000, 0, 0x801f_fff0).await.expect("RUN");
    let t0 = Instant::now();
    let deadline = deadline_after(Duration::from_millis(300));
    let r = s.run_until_stop(deadline, None).await.expect("run");
    assert_eq!(r.stop, None);
    assert_eq!(r.exit_code, None);
    assert!(t0.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn elf_and_cpe_images_load_and_run() {
    // A CPE whose load chunks are contiguous goes out as one segment.
    let text = program_text(5000);
    let (first, second) = text.split_at(3000);
    let mut cpe = b"CPE\x01\x08\x00\x03\x90\x00".to_vec();
    cpe.extend(0x8001_0000u32.to_le_bytes());
    for (addr, part) in [(0x8001_0000u32, first), (0x8001_0bb8, second)] {
        cpe.push(0x01);
        cpe.extend(addr.to_le_bytes());
        cpe.extend(len32(part).to_le_bytes());
        cpe.extend(part);
    }
    cpe.push(0x00);
    let img = exe::parse(&cpe).expect("parse CPE");
    assert_eq!(img.segments.len(), 1);
    let prog = CheckAndExit {
        addr: 0x8001_0000,
        expect: text.clone(),
        pc: 0x8001_0000,
        gp: 0,
        sp: exe::DEFAULT_STACK_ELF_CPE,
        code: 5,
        exit_at: 0x8001_2000,
        started: false,
    };
    let (mut s, _) = attach(SimConfig::default(), Box::new(prog)).await;
    for seg in &img.segments {
        s.load(seg.addr, &seg.data, &LoadOptions::default())
            .await
            .expect("load");
    }
    s.run(img.pc, img.gp, img.sp).await.expect("RUN");
    assert_eq!(run_until_stop(&mut s, 5, None).await.exit_code, Some(5));
}
