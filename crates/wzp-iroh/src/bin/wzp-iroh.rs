//! `wzp-iroh` — minimal CLI to demo identity, contacts, presence and text
//! signaling between two nodes.
//!
//! ```text
//! wzp-iroh --dir /tmp/a id
//! wzp-iroh --dir /tmp/b add-contact wzp1...
//! wzp-iroh --dir /tmp/a listen
//! wzp-iroh --dir /tmp/b send-text alice "hello"
//! ```

use std::{path::PathBuf, time::Duration};

use clap::{Parser, Subcommand};
use wzp_iroh::{
    Contact, ContactBook, ContactCard, Envelope, IrohConfig, IrohNode, NodeEvent, RelayChoice,
    identity,
};

#[derive(Parser)]
#[command(
    name = "wzp-iroh",
    version,
    about = "WarzonePhone iroh identity/contacts/signaling demo"
)]
struct Cli {
    /// Data directory (seed + contacts.json). Default: $WZP_IROH_DIR or ~/.wzp-iroh
    #[arg(long, global = true)]
    dir: Option<PathBuf>,
    /// Relay: "n0" (default), "disabled", or an iroh-relay URL (repeatable via commas)
    #[arg(long, global = true, default_value = "n0")]
    relay: String,
    /// Access token for a self-hosted iroh-relay (access.shared_token)
    #[arg(long, global = true, env = "WZP_IROH_RELAY_TOKEN")]
    relay_token: Option<String>,
    /// Disable mDNS local discovery
    #[arg(long, global = true)]
    no_mdns: bool,
    /// Disable DNS/pkarr address publishing and lookup (dns.iroh.link)
    #[arg(long, global = true)]
    no_dns: bool,
    /// Bind address, e.g. 0.0.0.0:7777
    #[arg(long, global = true)]
    bind: Option<std::net::SocketAddr>,
    /// Display name used in contact strings / Hello
    #[arg(long, global = true, default_value = "wzp")]
    name: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print our id, fingerprint and contact string
    Id {
        /// Also print the 24-word recovery phrase
        #[arg(long)]
        show_mnemonic: bool,
    },
    /// Add a contact from a wzp1… string (or a bare endpoint id)
    AddContact { contact: String },
    /// List contacts
    Contacts,
    /// Run the node and print incoming messages / presence / contact requests
    Listen,
    /// Send a text message. <contact> = name, id, or wzp1… string
    SendText { contact: String, msg: String },
    /// Introduce yourself to someone who has not added you yet
    Hello { contact: String },
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

fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("WZP_IROH_DIR") {
        return d.into();
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".wzp-iroh")
}

/// Load `<dir>/seed.hex`, creating a random seed on first use.
fn load_seed(dir: &std::path::Path) -> wzp_iroh::Result<[u8; 32]> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("seed.hex");
    if path.exists() {
        let s = std::fs::read_to_string(&path)?;
        let seed = wzp_crypto::Seed::from_hex(s.trim()).map_err(wzp_iroh::Error::Config)?;
        return Ok(seed.0);
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

fn config(cli: &Cli, dir: PathBuf) -> IrohConfig {
    let relay = match cli.relay.as_str() {
        "n0" | "default" => RelayChoice::N0,
        "disabled" | "none" | "off" => RelayChoice::Disabled,
        urls => RelayChoice::Custom(urls.split(',').map(|s| s.trim().to_string()).collect()),
    };
    IrohConfig {
        data_dir: Some(dir),
        relay,
        relay_auth_token: cli.relay_token.clone(),
        local_discovery: !cli.no_mdns,
        dns_discovery: !cli.no_dns,
        bind_addr: cli.bind,
        ..IrohConfig::default()
    }
}

async fn start(cli: &Cli, dir: PathBuf) -> wzp_iroh::Result<IrohNode> {
    let seed = load_seed(&dir)?;
    let cfg = config(cli, dir);
    let has_relay = cfg.relay != RelayChoice::Disabled;
    let node = IrohNode::start(&seed, cfg).await?;
    if has_relay
        && tokio::time::timeout(Duration::from_secs(8), node.endpoint().online())
            .await
            .is_err()
    {
        eprintln!("warning: no relay connection yet; continuing with direct addresses");
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
    let matches: Vec<_> = book.iter().filter(|c| c.name == s).collect();
    (matches.len() == 1).then(|| matches[0].clone())
}

fn print_contact(c: &Contact) {
    println!(
        "{}  {:<16} {}  trust={:?}",
        c.id, c.name, c.fingerprint, c.trust
    );
}

async fn run(cli: Cli) -> wzp_iroh::Result<()> {
    let dir = cli.dir.clone().unwrap_or_else(default_dir);
    match &cli.cmd {
        Cmd::Id { show_mnemonic } => {
            let seed = load_seed(&dir)?;
            let node = start(&cli, dir).await?;
            println!("endpoint id : {}", node.id());
            println!("id (z32)    : {}", node.id().to_z32());
            println!("fingerprint : {}", node.fingerprint());
            println!("contact     : {}", node.contact_string(&cli.name));
            if *show_mnemonic {
                println!(
                    "mnemonic    : {}",
                    wzp_crypto::Seed::from_bytes(seed).to_mnemonic()
                );
            }
            node.shutdown().await;
        }
        Cmd::AddContact { contact } => {
            // Offline operation: no need to start the network.
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
                    None => return Err(e),
                },
            };
            if card.id == me {
                return Err(wzp_iroh::Error::SelfContact);
            }
            let book = ContactBook::open(&dir)?;
            let c = book.upsert(Contact::from_card(&card))?;
            println!("added (trust on first use) — compare this fingerprint out of band:");
            print_contact(&c);
        }
        Cmd::Contacts => {
            for c in ContactBook::open(&dir)?.list() {
                print_contact(&c);
            }
        }
        Cmd::Listen => {
            let node = start(&cli, dir).await?;
            let mut rx = node.events();
            println!("listening as {} ({})", node.id(), node.fingerprint());
            println!("contact: {}", node.contact_string(&cli.name));
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    ev = rx.recv() => match ev {
                        Ok(NodeEvent::Message { from, env }) => {
                            let name = node.contacts().into_iter().find(|c| c.id == from).map(|c| c.name).unwrap_or_default();
                            match env {
                                Envelope::Text { text, .. } => println!("[{name}] {text}"),
                                other => println!("[{name}] {}", serde_json::to_string(&other).unwrap_or_default()),
                            }
                        }
                        Ok(NodeEvent::Presence { id, state, .. }) => println!("presence {} {:?}", id.fmt_short(), state),
                        Ok(NodeEvent::ContactRequest { id, fingerprint, name, contact }) => {
                            println!("contact request from {id} name={name:?} fingerprint={fingerprint}");
                            println!("  accept with: wzp-iroh add-contact {}", contact.unwrap_or_else(|| id.to_string()));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => break,
                    }
                }
            }
            node.shutdown().await;
        }
        Cmd::SendText { contact, msg } => {
            let node = start(&cli, dir).await?;
            if ContactCard::parse(contact).is_ok() {
                node.add_contact(contact).await?;
            }
            let c = resolve(&node.contacts(), contact).ok_or_else(|| {
                wzp_iroh::Error::Config(format!("no unique contact matching {contact:?}"))
            })?;
            let res = node.send(c.id, Envelope::text(msg.clone())).await;
            node.shutdown().await;
            res?;
            println!("delivered to {}", c.name);
        }
        Cmd::Hello { contact } => {
            let node = start(&cli, dir).await?;
            if ContactCard::parse(contact).is_ok() {
                node.add_contact(contact).await?;
            }
            let c = resolve(&node.contacts(), contact).ok_or_else(|| {
                wzp_iroh::Error::Config(format!("no unique contact matching {contact:?}"))
            })?;
            match node.send_hello(c.id, &cli.name).await {
                Ok(()) => println!("{} already has you as a contact", c.name),
                Err(wzp_iroh::Error::Rejected(..)) => {
                    println!("contact request sent to {}", c.name)
                }
                Err(e) => {
                    node.shutdown().await;
                    return Err(e);
                }
            }
            node.shutdown().await;
        }
    }
    Ok(())
}
