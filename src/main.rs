//! psxmon: command-line host for the PS1 debug monitor.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant as StdInstant};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use psxmon::pcdrv::{PcdrvServer, Quota};
use psxmon::proto::{self, CAP_LZ4, STOP_EXIT};
use psxmon::session::{LoadOptions, Session};
use psxmon::{SerialTransport, bios, exe, lz4};

/// Largest target exit code passed through as the process exit status;
/// anything above it (or negative) exits with this value.
const EXIT_CODE_MAX: u8 = 123;
/// Exit status when the target did not stop before --timeout.
const EXIT_TIMEOUT: u8 = 124;
/// Exit status on a host, link or protocol error.
const EXIT_ERROR: u8 = 125;
/// Exit status when the target stopped without exiting (fault, breakpoint).
const EXIT_STOPPED: u8 = 126;

#[derive(Parser)]
#[command(
    name = "psxmon",
    version,
    about = "Host tool for the PS1 debug monitor"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct Link {
    /// Serial device the monitor is on.
    #[arg(long, env = "PSXMON_PORT")]
    port: String,
    /// Line rate the monitor listens at after boot.
    #[arg(long, default_value_t = 115200)]
    baud: u32,
    /// SIO1 reload to switch to after attaching (9 = 230400, 5 = 414720).
    #[arg(long, value_name = "RELOAD")]
    fast_reload: Option<u16>,
    /// Seconds to PING for before giving up on the monitor.
    #[arg(long, value_name = "SECS", default_value_t = 5.0)]
    attach_timeout: f64,
}

#[derive(Args)]
struct RunArgs {
    /// Program to run (PS-EXE, ELF or CPE).
    file: PathBuf,
    #[command(flatten)]
    link: Link,
    /// Send the program LZ4-compressed when the monitor supports it (default).
    #[arg(long, overrides_with = "no_lz4")]
    lz4: bool,
    /// Send the program uncompressed.
    #[arg(long, overrides_with = "lz4")]
    no_lz4: bool,
    /// Longest LZ4 match copy per sequence.
    #[arg(long, value_name = "BYTES", default_value_t = lz4::DEFAULT_MAX_MATCH)]
    max_match: usize,
    /// Serve PCDRV file I/O from this directory.
    #[arg(long, value_name = "DIR")]
    pcdrv: Option<PathBuf>,
    /// Seconds to let the program run.
    #[arg(long, value_name = "SECS", default_value_t = 60.0)]
    timeout: f64,
    /// Log load details and PCDRV calls to stderr.
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Upload a program, run it, stream its console text to stdout, serve
    /// PCDRV, and exit with its exit code.
    Run(RunArgs),
    /// Print the monitor's protocol version, capabilities and BIOS.
    Ping {
        #[command(flatten)]
        link: Link,
    },
    /// Read target memory to a file.
    Dump {
        #[arg(value_parser = parse_u32)]
        addr: u32,
        #[arg(value_parser = parse_u32)]
        len: u32,
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
        #[command(flatten)]
        link: Link,
    },
    /// Write a file into target memory.
    Write {
        #[arg(value_parser = parse_u32)]
        addr: u32,
        file: PathBuf,
        #[command(flatten)]
        link: Link,
    },
}

fn parse_u32(s: &str) -> std::result::Result<u32, String> {
    let r = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(&hex.replace('_', ""), 16),
        None => s.replace('_', "").parse(),
    };
    r.map_err(|e| format!("{s}: {e}"))
}

fn seconds(secs: f64, what: &str) -> Result<Duration> {
    Duration::try_from_secs_f64(secs).with_context(|| format!("{what}: bad number of seconds"))
}

/// How long to PING at each rate other than --baud before trying the next.
const PROBE_WAIT: Duration = Duration::from_secs(1);

async fn attach(link: &Link) -> Result<Session<SerialTransport>> {
    let io = SerialTransport::open(&link.port, link.baud)
        .with_context(|| format!("opening {}", link.port))?;
    let mut s = Session::new(io);
    // A monitor an earlier SET_BAUD left at a faster rate does not answer at
    // --baud, so fall back to the rates it can have been left at: the boot
    // rate, 230400 (reload 9), and whatever --fast-reload names.
    let mut rates = vec![link.baud, proto::sio1_rate(18), proto::sio1_rate(9)];
    rates.extend(link.fast_reload.map(proto::sio1_rate));
    let mut seen = Vec::new();
    rates.retain(|r| {
        let fresh = !seen.contains(r);
        seen.push(*r);
        fresh
    });
    let first_wait = seconds(link.attach_timeout, "--attach-timeout")?;
    let Some(rate) = s.attach_at(&rates, first_wait, PROBE_WAIT).await? else {
        bail!(
            "no PONG from the monitor on {} at {rates:?} baud",
            link.port
        );
    };
    if rate != link.baud {
        eprintln!("psxmon: monitor answered at {rate} baud, not {}", link.baud);
    }
    // Anything before the first PONG is boot or line noise, not program text.
    s.take_text();
    if let Some(reload) = link.fast_reload {
        let rate = s.negotiate_rate(reload).await?;
        if rate != proto::sio1_rate(reload) {
            eprintln!("psxmon: reload {reload} did not answer, staying at {rate} baud");
        }
    }
    Ok(s)
}

fn describe_caps(caps: u16) -> String {
    let mut names = vec![];
    if caps & CAP_LZ4 != 0 {
        names.push("lz4");
    }
    format!(
        "0x{caps:04x} ({})",
        if names.is_empty() {
            "none".into()
        } else {
            names.join(", ")
        }
    )
}

async fn ping(link: Link) -> Result<ExitCode> {
    let s = attach(&link).await?;
    match s.version {
        Some(v) => println!("protocol {v}"),
        None => println!("protocol unknown"),
    }
    println!("caps {}", describe_caps(s.caps));
    match s.bios {
        Some(b) => println!("bios 0x{b:08x} {}", bios::bios_name(b)),
        None => println!("bios not reported"),
    }
    Ok(ExitCode::SUCCESS)
}

async fn run(args: RunArgs) -> Result<ExitCode> {
    let RunArgs {
        file,
        link,
        lz4: _,
        no_lz4,
        max_match,
        pcdrv,
        timeout,
        verbose,
    } = args;
    let lz4 = !no_lz4;
    if max_match < 7 {
        bail!("--max-match must be at least 7");
    }
    let image = exe::load(&file).with_context(|| format!("loading {}", file.display()))?;
    let mut server = match pcdrv {
        Some(dir) => Some(
            PcdrvServer::new(&dir, Quota::default())
                .with_context(|| format!("PCDRV dir {}", dir.display()))?,
        ),
        None => None,
    };
    let mut s = attach(&link).await?;
    s.verbose = verbose;
    let opts = LoadOptions {
        lz4,
        max_match,
        ..Default::default()
    };
    let t0 = StdInstant::now();
    for seg in &image.segments {
        let st = s.load(seg.addr, &seg.data, &opts).await?;
        if verbose {
            let how = st
                .lz4_bytes
                .map_or("plain".to_string(), |n| format!("lz4 {n} bytes"));
            eprintln!(
                "psxmon: loaded {} bytes at 0x{:08x} ({how})",
                st.bytes, seg.addr
            );
        }
    }
    if verbose {
        eprintln!(
            "psxmon: load took {} ms at {} baud; run pc=0x{:08x} gp=0x{:08x} sp=0x{:08x}",
            t0.elapsed().as_millis(),
            psxmon::Transport::baud_rate(s.transport()),
            image.pc,
            image.gp,
            image.sp
        );
    }
    s.set_console(Some(Box::new(|bytes: &[u8]| {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    })));
    let deadline = psxmon::session::deadline_after(seconds(timeout, "--timeout")?);
    s.run(image.pc, image.gp, image.sp).await?;
    let result = s.run_until_stop(deadline, server.as_mut()).await?;
    if let Some(sv) = server.as_mut() {
        sv.close_all();
    }
    Ok(match (result.stop, result.exit_code) {
        (_, Some(code)) => {
            eprintln!("psxmon: exit code {code} (0x{code:x})");
            ExitCode::from(exit_status(code))
        }
        (Some(stop), None) => {
            debug_assert_ne!(stop.reason, STOP_EXIT);
            eprintln!(
                "psxmon: target stopped: {} at 0x{:08x}, a=0x{:08x} b=0x{:08x}",
                stop.reason_name(),
                stop.epc,
                stop.a,
                stop.b
            );
            ExitCode::from(EXIT_STOPPED)
        }
        (None, None) => {
            eprintln!("psxmon: timed out after {timeout} s");
            ExitCode::from(EXIT_TIMEOUT)
        }
    })
}

async fn dump(addr: u32, len: u32, output: PathBuf, link: Link) -> Result<ExitCode> {
    let mut s = attach(&link).await?;
    let data = s.read_mem(addr, len).await?;
    std::fs::write(&output, &data[..len as usize])
        .with_context(|| format!("writing {}", output.display()))?;
    eprintln!(
        "psxmon: read {len} bytes at 0x{addr:08x} to {}",
        output.display()
    );
    Ok(ExitCode::SUCCESS)
}

async fn write(addr: u32, file: PathBuf, link: Link) -> Result<ExitCode> {
    let data = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
    let mut s = attach(&link).await?;
    s.write_mem(addr, &data).await?;
    eprintln!("psxmon: wrote {} bytes at 0x{addr:08x}", data.len());
    Ok(ExitCode::SUCCESS)
}

/// The process exit status for a target exit code: the code itself when it
/// is 0..=123, else 123, so it never reads as one of psxmon's own statuses.
fn exit_status(code: u32) -> u8 {
    u8::try_from(code)
        .ok()
        .filter(|&c| c <= EXIT_CODE_MAX)
        .unwrap_or(EXIT_CODE_MAX)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Run(args) => run(args).await,
        Cmd::Ping { link } => ping(link).await,
        Cmd::Dump {
            addr,
            len,
            output,
            link,
        } => dump(addr, len, output, link).await,
        Cmd::Write { addr, file, link } => write(addr, file, link).await,
    };
    result.unwrap_or_else(|e| {
        eprintln!("psxmon: {e:#}");
        ExitCode::from(EXIT_ERROR)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_passes_small_codes_and_caps_the_rest() {
        assert_eq!(exit_status(0), 0);
        assert_eq!(exit_status(42), 42);
        assert_eq!(exit_status(123), 123);
        assert_eq!(exit_status(124), EXIT_CODE_MAX);
        assert_eq!(exit_status(20000), EXIT_CODE_MAX);
        assert_eq!(exit_status(u32::MAX), EXIT_CODE_MAX);
        assert_eq!(parse_u32("0x8001_0000"), Ok(0x8001_0000));
        assert_eq!(parse_u32("4096"), Ok(4096));
        assert!(parse_u32("0x1_0000_0000").is_err());
    }
}
