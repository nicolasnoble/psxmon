//! Session behaviour against the simulated monitor.

mod sim;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use psxmon::exe;
use psxmon::pcdrv::{PcdrvServer, Quota};
use psxmon::proto::*;
use psxmon::session::{LoadOptions, Session};
use psxmon::{MemTransport, Transport};
use sim::*;
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

const BIOS: u32 = 0xbf38df5e;

async fn attach(cfg: SimConfig, program: Box<dyn Program>) -> (Session<MemTransport>, Arc<Mutex<SimStats>>) {
    let (host, stats) = sim::start(cfg, program);
    let mut s = Session::new(host);
    assert!(s.ping(Duration::from_secs(2), &[]).await.unwrap(), "no PONG");
    assert_eq!(s.version, Some(PROTO_VER));
    (s, stats)
}

/// Code-like bytes that compress but not trivially.
fn program_text(len: usize) -> Vec<u8> {
    let mut x: u32 = 7;
    (0..len)
        .map(|i| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if (i / 512) % 3 == 0 {
                0
            } else if (i / 64) % 2 == 0 {
                (i % 16) as u8
            } else {
                (x >> 24) as u8
            }
        })
        .collect()
}

async fn load_run_exit(lz4: bool) {
    let text = program_text(100_000);
    let file = exe::build_ps_exe(&text, 0x8001_0000, 0x8001_0000, 0x8009_0000, 0x801f_0000);
    let img = exe::parse(&file).unwrap();
    let seg = img.segments[0].clone();
    let prog = CheckAndExit {
        addr: seg.addr,
        expect: seg.data.clone(),
        pc: img.pc,
        gp: img.gp,
        sp: img.sp,
        code: 42,
        started: false,
    };
    let (mut s, stats) = attach(SimConfig::default(), Box::new(prog)).await;
    assert_eq!(s.caps, CAP_LZ4);
    assert_eq!(s.bios, Some(BIOS));
    let opts = LoadOptions {
        lz4,
        ..Default::default()
    };
    let st = s.load(seg.addr, &seg.data, &opts).await.unwrap();
    assert_eq!(st.lz4_bytes.is_some(), lz4);
    s.run(img.pc, img.gp, img.sp).await.unwrap();
    let r = s
        .run_until_stop(Instant::now() + Duration::from_secs(5), None)
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(42));
    let stop = r.stop.unwrap();
    assert_eq!(stop.reason, STOP_EXIT);
    assert_eq!(stop.epc, img.pc + 0x100);
    assert_eq!(s.take_text(), b"target: hello\n");
    let st = stats.lock().unwrap();
    if lz4 {
        assert!(st.lz4_frames >= 2, "lz4 frames {}", st.lz4_frames);
        assert_eq!(st.plain_load_frames, 0);
        assert!(st.max_match > 0 && st.max_match <= 128, "max match {}", st.max_match);
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
        .unwrap();
    assert_eq!(st.lz4_bytes, None);
    assert_eq!(stats.lock().unwrap().plain_load_frames, 3);
}

async fn farmjob(lz4: bool, size: usize) {
    let dir = tempfile::tempdir().unwrap();
    let input: Vec<u8> = (0..size).map(|i| b"abcdefghijklmnopqrstuvwxyz\n"[i % 27]).collect();
    std::fs::write(dir.path().join("IN.TXT"), &input).unwrap();
    let (mut s, _stats) = attach(SimConfig::default(), Box::new(Farmjob::new())).await;
    // A stand-in body so the load path runs too.
    let body = program_text(30_000);
    s.load(
        0x8001_0000,
        &body,
        &LoadOptions {
            lz4,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut server = PcdrvServer::new(dir.path(), Quota::default()).unwrap();
    s.run(0x8001_0000, 0, 0x801f_fff0).await.unwrap();
    let r = s
        .run_until_stop(Instant::now() + Duration::from_secs(10), Some(&mut server))
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(size as u32));
    assert_eq!(
        std::fs::read(dir.path().join("OUT.TXT")).unwrap(),
        input.to_ascii_uppercase()
    );
    let text = String::from_utf8(s.take_text()).unwrap();
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
    s.run(0x8001_0000, 0, 0x801f_fff0).await.unwrap();
    let r = s
        .run_until_stop(Instant::now() + Duration::from_secs(5), None)
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(0xbad));
}

#[tokio::test]
async fn pcdrv_jail_refuses_escape() {
    let outer = tempfile::tempdir().unwrap();
    let base = outer.path().join("jail");
    std::fs::create_dir(&base).unwrap();
    let (mut s, _) = attach(
        SimConfig::default(),
        Box::new(JailProbe {
            name: b"..\\escaped.txt",
            phase: 0,
        }),
    )
    .await;
    let mut server = PcdrvServer::new(&base, Quota::default()).unwrap();
    s.run(0x8001_0000, 0, 0x801f_fff0).await.unwrap();
    let r = s
        .run_until_stop(Instant::now() + Duration::from_secs(5), Some(&mut server))
        .await
        .unwrap();
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
        started: false,
    };
    let (mut s, _) = attach(cfg, Box::new(prog)).await;
    s.run(0x8001_0000, 0, 0x801f_fff0).await.unwrap();
    let r = s
        .run_until_stop(Instant::now() + Duration::from_secs(5), None)
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(7));
    let stop = r.stop.unwrap();
    assert_eq!(stop.reason, STOP_EXIT);
    assert_eq!(stop.a, 7);
}

#[tokio::test]
async fn set_baud_short_pong_keeps_caps_and_bios() {
    let (mut s, stats) = attach(SimConfig::default(), Box::new(Hang)).await;
    assert_eq!((s.caps, s.bios), (CAP_LZ4, Some(BIOS)));
    let rate = s.negotiate_rate(9).await.unwrap();
    assert_eq!(rate, 230400);
    assert_eq!(s.transport().baud_rate(), 230400);
    assert_eq!(stats.lock().unwrap().rate, 230400);
    // The two window PONGs carried only [proto_ver].
    assert_eq!((s.caps, s.bios), (CAP_LZ4, Some(BIOS)));
    assert!(s.ping(Duration::from_secs(1), &[]).await.unwrap());
    let data = program_text(20000);
    s.write_mem(0x8002_0000, &data).await.unwrap();
    assert_eq!(s.read_mem(0x8002_0000, data.len() as u32).await.unwrap(), data);
}

#[tokio::test]
async fn set_baud_falls_back_when_new_rate_is_silent() {
    let cfg = SimConfig {
        unreachable_rate: true,
        ..Default::default()
    };
    let (mut s, stats) = attach(cfg, Box::new(Hang)).await;
    let rate = s.negotiate_rate(9).await.unwrap();
    assert_eq!(rate, 115200);
    assert_eq!(s.transport().baud_rate(), 115200);
    assert_eq!(stats.lock().unwrap().rate, 115200);
    assert_eq!((s.caps, s.bios), (CAP_LZ4, Some(BIOS)));
}

#[tokio::test]
async fn memory_regs_and_errors() {
    let (mut s, stats) = attach(SimConfig::default(), Box::new(Hang)).await;
    // Across several DATA frames, an odd length, and len 0.
    let data = program_text(3 * 8192 + 5);
    s.write_mem(0x8003_0001, &data).await.unwrap();
    assert_eq!(s.read_mem(0x8003_0001, data.len() as u32).await.unwrap(), data);
    assert_eq!(s.read_mem(0x8003_0001, 0).await.unwrap(), Vec::<u8>::new());
    // No halted context yet.
    assert!(s.get_regs().await.unwrap().iter().all(|&r| r == 0));
    let e = s.set_reg(2, 1).await.unwrap_err();
    assert!(e.to_string().contains("EBADSTATE"), "{e}");
    let e = s.cont().await.unwrap_err();
    assert!(e.to_string().contains("EBADSTATE"), "{e}");
    // A corrupted command frame is answered with ECKSUM.
    let mut bad = psxmon::frame::encode_frame(GET_REGS, &[]);
    let n = bad.len();
    bad[n - 1] ^= 0x40;
    s.transport().write_all(&bad).await.unwrap();
    let f = s
        .wait_frame(&[ERROR], Instant::now() + Duration::from_secs(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.words, vec![E_CKSUM]);
    // STEP is reserved.
    s.send(STEP, &[]).await.unwrap();
    let f = s
        .wait_frame(&[ERROR], Instant::now() + Duration::from_secs(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.words, vec![E_BADCMD]);
    assert_eq!(
        stats.lock().unwrap().errors_sent,
        vec![E_BADSTATE, E_BADSTATE, E_CKSUM, E_BADCMD]
    );
}

#[tokio::test]
async fn timeout_when_target_never_stops() {
    let (mut s, _) = attach(SimConfig::default(), Box::new(Hang)).await;
    s.run(0x8001_0000, 0, 0x801f_fff0).await.unwrap();
    let t0 = Instant::now();
    let r = s
        .run_until_stop(Instant::now() + Duration::from_millis(300), None)
        .await
        .unwrap();
    assert_eq!(r.stop, None);
    assert_eq!(r.exit_code, None);
    assert!(t0.elapsed() >= Duration::from_millis(300));
}
