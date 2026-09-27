//! `iso::build` against PCSX-Redux's exe2iso, byte for byte. The oracle is
//! a local build; without it (as in CI) the comparisons are skipped.

use std::error::Error;
use std::path::Path;
use std::process::Command;

use psxmon::exe::build_ps_exe;
use psxmon::iso;

type Res<T> = Result<T, Box<dyn Error>>;

/// A case: its name, the PS-EXE, and the license file if any.
type Case<'a> = (&'a str, Vec<u8>, Option<&'a [u8]>);

const ORACLE: &str = "/home/pixel/sources/pcsx-redux/bins/Release/exe2iso";

/// Deterministic filler bytes (xorshift32).
fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x.to_le_bytes()[0]
        })
        .collect()
}

fn ps_exe(text_len: usize, seed: u32) -> Res<Vec<u8>> {
    Ok(build_ps_exe(
        &noise(text_len, seed),
        0x8001_0000,
        0x8001_0000,
        0,
        0x801f_ff00,
    )?)
}

fn oracle(dir: &Path, exe: &[u8], license: Option<&[u8]>, pad: bool) -> Res<Vec<u8>> {
    let exe_path = dir.join("in.ps-exe");
    let out = dir.join("oracle.bin");
    std::fs::write(&exe_path, exe)?;
    let mut cmd = Command::new(ORACLE);
    cmd.arg(&exe_path);
    if let Some(l) = license {
        let p = dir.join("license.dat");
        std::fs::write(&p, l)?;
        cmd.arg("-license").arg(p);
    }
    if !pad {
        cmd.arg("-nopad");
    }
    cmd.arg("-o").arg(&out);
    let res = cmd.output()?;
    assert!(res.status.success(), "exe2iso failed: {res:?}");
    Ok(std::fs::read(&out)?)
}

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .or_else(|| (a.len() != b.len()).then(|| a.len().min(b.len())))
}

#[test]
fn mkdisc_matches_exe2iso() -> Res<()> {
    if !Path::new(ORACLE).exists() {
        println!("mkdisc_oracle: SKIP, no exe2iso at {ORACLE}");
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");

    let mut sdk = noise(16 * 2336, 7);
    sdk[0x2492] = b'L';
    let mut sdk_short = sdk.clone();
    sdk_short.truncate(20_000);
    let mut raw = noise(16 * 2352, 9);
    raw[0x2492] = b'x';
    raw[0x24e2] = b'L';
    let mut unknown = noise(16 * 2352, 11);
    unknown[0x2492] = b'x';
    unknown[0x24e2] = b'x';

    let small = ps_exe(100, 1)?;
    let large = ps_exe(300 * 1024, 2)?;
    let odd = noise(5_000, 3);
    let cases: [Case; 8] = [
        ("small ps-exe", small.clone(), None),
        ("large ps-exe", large.clone(), None),
        ("odd-size file", odd, None),
        ("small ps-exe, sdk 2336 license", small.clone(), Some(&sdk)),
        (
            "small ps-exe, short sdk license",
            small.clone(),
            Some(&sdk_short),
        ),
        ("large ps-exe, raw 2352 license", large, Some(&raw)),
        (
            "small ps-exe, unrecognised license",
            small.clone(),
            Some(&unknown),
        ),
        ("small ps-exe, empty license", small, Some(&[])),
    ];
    let mut compared = 0u32;
    for (name, exe, license) in &cases {
        for pad in [true, false] {
            let want = oracle(dir.path(), exe, *license, pad)?;
            let got = iso::build(exe, *license, pad).expect("build");
            assert_eq!(
                first_difference(&got, &want),
                None,
                "{name}, pad {pad}: lengths {} vs oracle {}",
                got.len(),
                want.len()
            );
            println!(
                "mkdisc_oracle: {name}, pad {pad}: {} bytes ({} sectors) identical",
                got.len(),
                got.len() / iso::SECTOR_RAW
            );
            compared = compared.wrapping_add(1);
        }
    }
    assert_eq!(compared, 16);
    Ok(())
}

#[test]
fn mkdisc_cli_writes_bin_and_cue() -> Res<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = ps_exe(4000, 5)?;
    let exe_path = dir.path().join("prog.ps-exe");
    std::fs::write(&exe_path, &exe).expect("write exe");
    let bin = dir.path().join("disc.bin");
    let res = Command::new(env!("CARGO_BIN_EXE_psxmon"))
        .arg("mkdisc")
        .arg(&exe_path)
        .arg("-o")
        .arg(&bin)
        .arg("--no-pad")
        .output()
        .expect("run psxmon");
    assert!(res.status.success(), "psxmon mkdisc failed: {res:?}");
    let got = std::fs::read(&bin).expect("read bin");
    assert_eq!(got, iso::build(&exe, None, false).expect("build"));
    let cue = std::fs::read_to_string(dir.path().join("disc.cue")).expect("read cue");
    assert_eq!(
        cue,
        "FILE \"disc.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n"
    );
    if Path::new(ORACLE).exists() {
        assert_eq!(got, oracle(dir.path(), &exe, None, false)?);
        println!("mkdisc_oracle: cli --no-pad: {} bytes identical", got.len());
    }
    Ok(())
}
