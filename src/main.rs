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
use tokio::time::Instant;

/// Exit status when the target did not stop before --timeout.
const EXIT_TIMEOUT: u8 = 124;
/// Exit status on a host, link or protocol error.
const EXIT_ERROR: u8 = 125;
/// Exit status when the target stopped without exiting (fault, breakpoint).
const EXIT_STOPPED: u8 = 126;

#[derive(Parser)]
#[command(name = "psxmon", version, about = "Host tool for the PS1 debug monitor")]
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

#[derive(Subcommand)]
enum Cmd {
    /// Upload a program, run it, stream its console text to stdout, serve
    /// PCDRV, and exit with its exit code.
    Run {
        /// Program to run (PS-EXE).
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
    },
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

async fn attach(link: &Link) -> Result<Session<SerialTransport>> {
    let io = SerialTransport::open(&link.port, link.baud).with_context(|| format!("opening {}", link.port))?;
    let mut s = Session::new(io);
    if !s.ping(Duration::from_secs_f64(link.attach_timeout), &[]).await? {
        bail!("no PONG from the monitor on {} at {} baud", link.port, link.baud);
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

#[allow(clippy::too_many_arguments)]
async fn run(
    file: PathBuf,
    link: Link,
    lz4: bool,
    max_match: usize,
    pcdrv: Option<PathBuf>,
    timeout: f64,
    verbose: bool,
) -> Result<ExitCode> {
    if max_match < 7 {
        bail!("--max-match must be at least 7");
    }
    let image = exe::load(&file).with_context(|| format!("loading {}", file.display()))?;
    let mut server = match pcdrv {
        Some(dir) => {
            Some(PcdrvServer::new(&dir, Quota::default()).with_context(|| format!("PCDRV dir {}", dir.display()))?)
        }
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
            let how = st.lz4_bytes.map_or("plain".to_string(), |n| format!("lz4 {n} bytes"));
            eprintln!("psxmon: loaded {} bytes at 0x{:08x} ({how})", st.bytes, seg.addr);
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
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    s.run(image.pc, image.gp, image.sp).await?;
    let result = s.run_until_stop(deadline, server.as_mut()).await?;
    if let Some(sv) = server.as_mut() {
        sv.close_all();
    }
    Ok(match (result.stop, result.exit_code) {
        (_, Some(code)) => {
            eprintln!("psxmon: exit code {code} (0x{code:x})");
            ExitCode::from(code as u8)
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
    std::fs::write(&output, &data[..len as usize]).with_context(|| format!("writing {}", output.display()))?;
    eprintln!("psxmon: read {len} bytes at 0x{addr:08x} to {}", output.display());
    Ok(ExitCode::SUCCESS)
}

async fn write(addr: u32, file: PathBuf, link: Link) -> Result<ExitCode> {
    let data = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
    let mut s = attach(&link).await?;
    s.write_mem(addr, &data).await?;
    eprintln!("psxmon: wrote {} bytes at 0x{addr:08x}", data.len());
    Ok(ExitCode::SUCCESS)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Run {
            file,
            link,
            lz4: _,
            no_lz4,
            max_match,
            pcdrv,
            timeout,
            verbose,
        } => run(file, link, !no_lz4, max_match, pcdrv, timeout, verbose).await,
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
