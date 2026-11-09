//! `wzp-share` — send files to contacts (iroh signaling + WZP file sessions).
//!
//! ```text
//! wzp-share --dir /tmp/a id
//! wzp-share --dir /tmp/b add-contact wzp1...
//! wzp-share --dir /tmp/b listen --auto-accept
//! wzp-share --dir /tmp/a --relay 203.0.113.1:4433 send bob ./movie.mkv
//! ```

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use clap::{Parser, Subcommand};
use tokio::io::AsyncBufReadExt;
use wzp_iroh::{Contact, ContactBook, ContactCard, identity};
use wzp_share::{
    Direction, IrohConfig, RelayChoice, ShareConfig, ShareEvent, ShareId, ShareNode, TransferInfo,
};

#[derive(Parser)]
#[command(
    name = "wzp-share",
    version,
    about = "Send files to WarzonePhone contacts"
)]
struct Cli {
    /// Data directory (seed, contacts.json, downloads/). Default: $WZP_SHARE_DIR or ~/.wzp-share
    #[arg(long, global = true)]
    dir: Option<PathBuf>,
    /// WZP relay used as fallback for file sessions (host:port)
    #[arg(long, global = true, env = "WZP_SHARE_RELAY")]
    relay: Option<SocketAddr>,
    /// Secret access token for the WZP relay (needed for the relay fallback;
    /// never sent to peers). Prefer the env var over the command line.
    #[arg(long, global = true, env = "WZP_SHARE_RELAY_TOKEN", hide_env_values = true)]
    relay_token: Option<String>,
    /// iroh relay: "n0" (default), "disabled", or iroh-relay URL(s), comma separated
    #[arg(long, global = true, default_value = "n0")]
    iroh_relay: String,
    /// Access token for a self-hosted iroh-relay
    #[arg(long, global = true)]
    iroh_relay_token: Option<String>,
    /// Disable mDNS local discovery
    #[arg(long, global = true)]
    no_mdns: bool,
    /// Disable DNS/pkarr discovery (dns.iroh.link)
    #[arg(long, global = true)]
    no_dns: bool,
    /// iroh bind address
    #[arg(long, global = true)]
    bind: Option<SocketAddr>,
    /// Bind address for direct file listeners ("none" disables the direct path)
    #[arg(long, global = true, default_value = "0.0.0.0:0")]
    direct_bind: String,
    /// Display name in contact strings
    #[arg(long, global = true, default_value = "wzp")]
    name: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print id, fingerprint and contact string
    Id,
    /// Add a contact (wzp1… string or endpoint id); works offline
    AddContact { contact: String },
    /// List contacts
    Contacts,
    /// Receive files (and print presence / contact requests)
    Listen {
        /// Accept all offers from contacts without asking
        #[arg(long)]
        auto_accept: bool,
        /// Where to store received files (default: <dir>/downloads)
        #[arg(long)]
        downloads: Option<PathBuf>,
    },
    /// Send a file. <contact> = name, id or wzp1… string
    Send { contact: String, path: PathBuf },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("WZP_SHARE_DIR") {
        return d.into();
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".wzp-share")
}

fn load_seed(dir: &std::path::Path) -> Result<[u8; 32], BoxErr> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("seed.hex");
    if path.exists() {
        let s = std::fs::read_to_string(&path)?;
        return Ok(wzp_crypto::Seed::from_hex(s.trim())?.0);
    }
    let seed = wzp_crypto::Seed::generate();
    std::fs::write(&path, hex::encode(seed.0))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("created new identity seed at {}", path.display());
    Ok(seed.0)
}

fn config(
    cli: &Cli,
    dir: PathBuf,
    auto_accept: bool,
    downloads: Option<PathBuf>,
) -> Result<ShareConfig, BoxErr> {
    let mut c = ShareConfig::new(&dir);
    let relay = match cli.iroh_relay.as_str() {
        "n0" | "default" => RelayChoice::N0,
        "disabled" | "none" | "off" => RelayChoice::Disabled,
        urls => RelayChoice::Custom(urls.split(',').map(|s| s.trim().to_string()).collect()),
    };
    c.iroh = IrohConfig {
        data_dir: Some(dir),
        relay,
        relay_auth_token: cli.iroh_relay_token.clone(),
        local_discovery: !cli.no_mdns,
        dns_discovery: !cli.no_dns,
        bind_addr: cli.bind,
        ..IrohConfig::default()
    };
    c.relay = cli.relay;
    c.relay_token = cli.relay_token.clone();
    c.direct_bind = match cli.direct_bind.as_str() {
        "none" | "off" => None,
        s => Some(s.parse()?),
    };
    c.auto_accept = auto_accept;
    if let Some(d) = downloads {
        c.files.download_dir = d;
    }
    Ok(c)
}

async fn start(
    cli: &Cli,
    dir: PathBuf,
    auto: bool,
    downloads: Option<PathBuf>,
) -> Result<ShareNode, BoxErr> {
    let seed = load_seed(&dir)?;
    let cfg = config(cli, dir, auto, downloads)?;
    let has_relay = cfg.iroh.relay != RelayChoice::Disabled;
    let node = ShareNode::start(&seed, cfg).await?;
    if has_relay
        && tokio::time::timeout(Duration::from_secs(8), node.iroh().endpoint().online())
            .await
            .is_err()
    {
        eprintln!("warning: no iroh relay connection yet; continuing");
    }
    Ok(node)
}

fn resolve(book: &[Contact], s: &str) -> Option<Contact> {
    if let Ok(card) = ContactCard::parse(s) {
        return book.iter().find(|c| c.id == card.id).cloned();
    }
    if let Some(id) = identity::parse_endpoint_id(s) {
        return book.iter().find(|c| c.id == id).cloned();
    }
    let m: Vec<_> = book.iter().filter(|c| c.name == s).collect();
    (m.len() == 1).then(|| m[0].clone())
}

fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

fn describe(t: &TransferInfo) -> String {
    let dir = match t.direction {
        Direction::Outgoing => "to",
        Direction::Incoming => "from",
    };
    let via = match t.relayed {
        Some(true) => " via relay",
        Some(false) => " direct",
        None => "",
    };
    let sas = t.sas.map(|s| format!(" code {s:04}")).unwrap_or_default();
    format!(
        "{} {dir} {} ({}){via}{sas}",
        t.name,
        t.contact_name,
        human(t.size)
    )
}

fn print_event(ev: &ShareEvent) {
    match ev {
        ShareEvent::Presence { id, state, .. } => {
            println!("presence {} {:?}", id.fmt_short(), state)
        }
        ShareEvent::ContactRequest {
            id,
            fingerprint,
            name,
            contact,
        } => {
            println!("contact request from {id} name={name:?} fingerprint={fingerprint}");
            println!(
                "  accept with: wzp-share add-contact {}",
                contact.clone().unwrap_or_else(|| id.to_string())
            );
        }
        ShareEvent::Text { from, text, .. } => println!("[{}] {text}", from.fmt_short()),
        ShareEvent::TransferOffered { transfer } => {
            println!("[{}] offered: {}", transfer.id, describe(transfer))
        }
        ShareEvent::TransferStarted { transfer } => {
            println!("[{}] started: {}", transfer.id, describe(transfer))
        }
        ShareEvent::TransferProgress { id, done, total } => {
            let pct = if *total == 0 { 100 } else { done * 100 / total };
            println!("[{id}] {pct:3}% {} / {}", human(*done), human(*total));
        }
        ShareEvent::TransferCompleted { transfer } => println!(
            "[{}] completed: {} -> {}",
            transfer.id,
            describe(transfer),
            transfer
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
        ShareEvent::TransferRejected { transfer } => {
            println!(
                "[{}] rejected: {}",
                transfer.id,
                transfer.error.clone().unwrap_or_default()
            )
        }
        ShareEvent::TransferFailed { transfer } => {
            println!(
                "[{}] failed: {}",
                transfer.id,
                transfer.error.clone().unwrap_or_default()
            )
        }
        ShareEvent::TransferCancelled { transfer, by_peer } => {
            println!(
                "[{}] cancelled{}",
                transfer.id,
                if *by_peer { " by peer" } else { "" }
            )
        }
        ShareEvent::InviteRefused { from, reason } => {
            println!("refused invite from {}: {reason}", from.fmt_short())
        }
    }
}

async fn run(cli: Cli) -> Result<(), BoxErr> {
    let dir = cli.dir.clone().unwrap_or_else(default_dir);
    match &cli.cmd {
        Cmd::Id => {
            let node = start(&cli, dir, false, None).await?;
            println!("endpoint id : {}", node.id());
            println!("fingerprint : {}", node.fingerprint());
            println!("contact     : {}", node.contact_string(&cli.name));
            node.shutdown().await;
        }
        Cmd::AddContact { contact } => {
            let seed = load_seed(&dir)?;
            let me = identity::secret_key_from_seed(&seed).public();
            let card = match ContactCard::parse(contact) {
                Ok(c) => c,
                Err(e) => match identity::parse_endpoint_id(contact) {
                    Some(id) => ContactCard {
                        id,
                        name: String::new(),
                        relay_url: None,
                        direct_addrs: vec![],
                    },
                    None => return Err(e.into()),
                },
            };
            if card.id == me {
                return Err("that is your own contact string".into());
            }
            let c = ContactBook::open(&dir)?.upsert(Contact::from_card(&card))?;
            println!("added (trust on first use) — compare fingerprint out of band:");
            println!("{}  {}  {}", c.id, c.name, c.fingerprint);
        }
        Cmd::Contacts => {
            for c in ContactBook::open(&dir)?.list() {
                println!(
                    "{}  {:<16} {}  trust={:?}",
                    c.id, c.name, c.fingerprint, c.trust
                );
            }
        }
        Cmd::Listen {
            auto_accept,
            downloads,
        } => {
            let node = start(&cli, dir, *auto_accept, downloads.clone()).await?;
            let mut rx = node.events();
            println!("listening as {} ({})", node.id(), node.fingerprint());
            println!("contact: {}", node.contact_string(&cli.name));
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            let mut pending: std::collections::VecDeque<ShareId> = Default::default();
            let mut stdin_open = true;
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    ev = rx.recv() => match ev {
                        Ok(ev) => {
                            print_event(&ev);
                            if let ShareEvent::TransferOffered { transfer } = &ev
                                && transfer.direction == Direction::Incoming
                                && !*auto_accept
                            {
                                pending.push_back(transfer.id);
                                println!("  accept? [y/N] (applies to the oldest pending offer)");
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => break,
                    },
                    line = stdin.next_line(), if stdin_open => match line {
                        Ok(Some(l)) => {
                            if let Some(id) = pending.pop_front() {
                                let r = if l.trim().eq_ignore_ascii_case("y") {
                                    node.accept(id).await
                                } else {
                                    node.reject(id, "declined").await
                                };
                                if let Err(e) = r { println!("[{id}] {e}"); }
                            }
                        }
                        _ => stdin_open = false,
                    },
                }
            }
            node.shutdown().await;
        }
        Cmd::Send { contact, path } => {
            let node = start(&cli, dir, false, None).await?;
            if ContactCard::parse(contact).is_ok() {
                node.add_contact(contact).await?;
            }
            let c = resolve(&node.contacts(), contact)
                .ok_or_else(|| format!("no unique contact matching {contact:?}"))?;
            let mut rx = node.events();
            let h = node.send_file(c.id, path).await?;
            let id = h.id();
            let printer = tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    let mine = match &ev {
                        ShareEvent::TransferProgress { id: i, .. } => *i == id,
                        ShareEvent::TransferOffered { transfer }
                        | ShareEvent::TransferStarted { transfer }
                        | ShareEvent::TransferCompleted { transfer }
                        | ShareEvent::TransferRejected { transfer }
                        | ShareEvent::TransferFailed { transfer }
                        | ShareEvent::TransferCancelled { transfer, .. } => transfer.id == id,
                        _ => false,
                    };
                    if mine {
                        print_event(&ev);
                    }
                }
            });
            let res = tokio::select! {
                r = h.wait() => r,
                _ = tokio::signal::ctrl_c() => {
                    let _ = node.cancel(id).await;
                    Err(wzp_share::ShareError::Cancelled)
                }
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
            printer.abort();
            node.shutdown().await;
            res?;
        }
    }
    Ok(())
}
