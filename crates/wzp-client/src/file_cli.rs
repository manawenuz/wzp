//! `wzp-client send-file` / `wzp-client receive-files` subcommands
//! (file transfer over WZP, see docs/FILE-TRANSFER.md).

use std::net::SocketAddr;
use std::path::PathBuf;

use tracing::info;
use wzp_files::{
    ConnectOptions, FileConfig, FileEndpoint, Fingerprint, Progress, Session, connect_direct,
    connect_via_relay_with, listen_direct, random_room,
};

const USAGE: &str = "\
Usage:
  wzp-client send-file <path>... (--relay <addr> [--room <token>] | --direct <addr>) [options]
  wzp-client receive-files (--relay <addr> --room <token> | --listen <addr>) [--dir <dir>] [--auto-accept] [options]

Options:
  --seed <hex> | --mnemonic <words...>   Identity (default: ephemeral)
  --peer <fingerprint>    Require this peer identity (hex, colons optional)
  --token <token>         Relay auth token
  --chunk-size <bytes>    Chunk size for sending (default 1 MiB)
  --max-size <bytes>      Max file size (default 10 GiB)
  --rate <bytes/s>        Cap send rate
";

#[derive(Default)]
struct FileArgs {
    paths: Vec<PathBuf>,
    relay: Option<SocketAddr>,
    room: Option<String>,
    direct: Option<SocketAddr>,
    listen: Option<SocketAddr>,
    dir: Option<PathBuf>,
    auto_accept: bool,
    seed_hex: Option<String>,
    mnemonic: Option<String>,
    peer: Option<String>,
    token: Option<String>,
    chunk_size: Option<u32>,
    max_size: Option<u64>,
    rate: Option<u64>,
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}\n\n{USAGE}");
    std::process::exit(2);
}

fn parse(args: &[String]) -> FileArgs {
    let mut a = FileArgs::default();
    let mut i = 0;
    let val = |i: &mut usize, flag: &str| -> String {
        *i += 1;
        args.get(*i)
            .cloned()
            .unwrap_or_else(|| die(&format!("{flag} requires a value")))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--relay" => {
                a.relay = Some(
                    val(&mut i, "--relay")
                        .parse()
                        .unwrap_or_else(|_| die("invalid --relay")),
                )
            }
            "--room" => a.room = Some(val(&mut i, "--room")),
            "--direct" => {
                a.direct = Some(
                    val(&mut i, "--direct")
                        .parse()
                        .unwrap_or_else(|_| die("invalid --direct")),
                )
            }
            "--listen" => {
                a.listen = Some(
                    val(&mut i, "--listen")
                        .parse()
                        .unwrap_or_else(|_| die("invalid --listen")),
                )
            }
            "--dir" => a.dir = Some(val(&mut i, "--dir").into()),
            "--auto-accept" => a.auto_accept = true,
            "--seed" => a.seed_hex = Some(val(&mut i, "--seed")),
            "--mnemonic" => {
                i += 1;
                let mut words = Vec::new();
                while i < args.len() && !args[i].starts_with('-') {
                    words.push(args[i].clone());
                    i += 1;
                }
                i -= 1;
                a.mnemonic = Some(words.join(" "));
            }
            "--peer" => a.peer = Some(val(&mut i, "--peer")),
            "--token" => a.token = Some(val(&mut i, "--token")),
            "--chunk-size" => {
                a.chunk_size = Some(
                    val(&mut i, "--chunk-size")
                        .parse()
                        .unwrap_or_else(|_| die("invalid --chunk-size")),
                )
            }
            "--max-size" => {
                a.max_size = Some(
                    val(&mut i, "--max-size")
                        .parse()
                        .unwrap_or_else(|_| die("invalid --max-size")),
                )
            }
            "--rate" => {
                a.rate = Some(
                    val(&mut i, "--rate")
                        .parse()
                        .unwrap_or_else(|_| die("invalid --rate")),
                )
            }
            "--help" | "-h" => {
                eprintln!("{USAGE}");
                std::process::exit(0);
            }
            other if other.starts_with('-') => die(&format!("unknown argument: {other}")),
            other => a.paths.push(other.into()),
        }
        i += 1;
    }
    a
}

impl FileArgs {
    fn seed(&self) -> wzp_crypto::Seed {
        let seed = if let Some(h) = &self.seed_hex {
            wzp_crypto::Seed::from_hex(h).unwrap_or_else(|e| die(&format!("invalid --seed: {e}")))
        } else if let Some(m) = &self.mnemonic {
            wzp_crypto::Seed::from_mnemonic(m)
                .unwrap_or_else(|e| die(&format!("invalid --mnemonic: {e}")))
        } else {
            wzp_crypto::Seed::generate()
        };
        eprintln!(
            "our fingerprint: {}",
            wzp_files::fingerprint_of_seed(&seed.0)
        );
        seed
    }

    fn opts(&self) -> ConnectOptions {
        ConnectOptions {
            expected_peer: self.peer.as_deref().map(|p| {
                Fingerprint::from_hex(p).unwrap_or_else(|e| die(&format!("invalid --peer: {e}")))
            }),
            auth_token: self.token.clone(),
            ..Default::default()
        }
    }

    fn config(&self) -> FileConfig {
        let mut c = FileConfig::default();
        if let Some(d) = &self.dir {
            c.download_dir = d.clone();
        }
        if let Some(cs) = self.chunk_size {
            c.chunk_size = cs;
        }
        if let Some(m) = self.max_size {
            c.max_size = m;
        }
        c.max_send_rate = self.rate;
        c
    }
}

fn print_session(s: &Session) {
    eprintln!(
        "connected to peer {} via {} — verify code: {:04}",
        s.peer_fingerprint(),
        if s.is_relayed() { "relay" } else { "direct" },
        s.sas()
    );
}

fn print_event(e: &Progress) {
    match e {
        Progress::Offered {
            transfer_id,
            name,
            size,
            direction,
        } => {
            eprintln!("[{transfer_id}] {direction:?} offer: {name} ({size} bytes)")
        }
        Progress::Started {
            transfer_id,
            resumed_bytes,
            ..
        } => {
            eprintln!("[{transfer_id}] started (resuming at {resumed_bytes} bytes)")
        }
        Progress::Bytes {
            transfer_id,
            done,
            total,
        } => {
            let pct = if *total > 0 {
                *done as f64 * 100.0 / *total as f64
            } else {
                100.0
            };
            eprintln!("[{transfer_id}] {done}/{total} bytes ({pct:.1}%)")
        }
        Progress::Completed { transfer_id, path } => {
            eprintln!("[{transfer_id}] completed: {}", path.display())
        }
        Progress::Rejected {
            transfer_id,
            reason,
        } => eprintln!("[{transfer_id}] rejected: {reason}"),
        Progress::Failed { transfer_id, error } => eprintln!("[{transfer_id}] failed: {error}"),
        Progress::Cancelled {
            transfer_id,
            by_peer,
        } => {
            eprintln!(
                "[{transfer_id}] cancelled{}",
                if *by_peer { " by peer" } else { "" }
            )
        }
    }
}

/// Run `send-file` / `receive-files` if `args[1]` names one. Returns
/// `Ok(true)` when a subcommand ran.
pub async fn maybe_run(args: &[String]) -> anyhow::Result<bool> {
    match args.get(1).map(String::as_str) {
        Some("send-file") => {
            send_files(parse(&args[2..])).await?;
            Ok(true)
        }
        Some("receive-files") => {
            receive_files(parse(&args[2..])).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

async fn send_files(a: FileArgs) -> anyhow::Result<()> {
    if a.paths.is_empty() {
        die("send-file needs at least one <path>");
    }
    let seed = a.seed();
    let session = if let Some(addr) = a.direct {
        connect_direct(addr, &seed.0, &a.opts()).await?
    } else if let Some(relay) = a.relay {
        let room = a.room.clone().unwrap_or_else(random_room);
        eprintln!(
            "room: {room}  (receiver: wzp-client receive-files --relay {relay} --room {room})"
        );
        connect_via_relay_with(relay, &room, &seed.0, &a.opts()).await?
    } else {
        die("send-file needs --relay or --direct");
    };
    print_session(&session);
    let ep = FileEndpoint::new(session, a.config());
    let mut events = ep.events();
    let printer = tokio::spawn(async move {
        while let Ok(e) = events.recv().await {
            print_event(&e);
        }
    });
    let mut failed = false;
    for p in &a.paths {
        let id = ep.send(p).await?;
        info!(transfer = %id, path = %p.display(), "offered");
        if let Err(e) = ep.wait(id).await {
            eprintln!("{}: {e}", p.display());
            failed = true;
        }
    }
    ep.close().await;
    printer.abort();
    if failed {
        anyhow::bail!("one or more transfers failed");
    }
    Ok(())
}

async fn receive_files(a: FileArgs) -> anyhow::Result<()> {
    let seed = a.seed();
    let session = if let Some(bind) = a.listen {
        let l = listen_direct(bind, &seed.0).await?;
        eprintln!("listening on {}", l.local_addr()?);
        l.accept(&a.opts()).await?
    } else if let Some(relay) = a.relay {
        let room = a
            .room
            .clone()
            .unwrap_or_else(|| die("receive-files needs --room with --relay"));
        connect_via_relay_with(relay, &room, &seed.0, &a.opts()).await?
    } else {
        die("receive-files needs --relay or --listen");
    };
    print_session(&session);
    let cfg = a.config();
    std::fs::create_dir_all(&cfg.download_dir)?;
    eprintln!("saving to {}", cfg.download_dir.display());
    let ep = FileEndpoint::new(session, cfg);
    let mut events = ep.events();
    let printer = tokio::spawn(async move {
        while let Ok(e) = events.recv().await {
            print_event(&e);
        }
    });
    while let Some(offer) = ep.next_offer().await {
        if a.auto_accept {
            eprintln!(
                "accepting {} ({} bytes) from {}",
                offer.name, offer.size, offer.peer
            );
            offer.accept();
            continue;
        }
        let prompt = format!(
            "accept {} ({} bytes) from {}? [y/N] ",
            offer.name, offer.size, offer.peer
        );
        let yes = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            eprint!("{prompt}");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            matches!(line.trim(), "y" | "Y" | "yes")
        })
        .await
        .unwrap_or(false);
        if yes {
            offer.accept();
        } else {
            offer.reject("declined by user");
        }
    }
    // Let the last events print.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    printer.abort();
    eprintln!("session closed");
    Ok(())
}
