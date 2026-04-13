//! WarzonePhone Protocol Analyzer — passive call quality observer.
//!
//! Joins a relay room as a passive participant (no media sent) and displays
//! real-time per-participant quality metrics in a terminal UI.
//!
//! Usage:
//!   wzp-analyzer 127.0.0.1:4433 --room test
//!   wzp-analyzer 1.2.3.4:4433 --room test --capture session.wzp
//!   wzp-analyzer 1.2.3.4:4433 --room test --no-tui --duration 60

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use tracing::info;

use wzp_proto::{CodecId, MediaPacket, MediaTransport};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// WarzonePhone Protocol Analyzer — passive call quality observer
#[derive(Parser)]
#[command(name = "wzp-analyzer", version)]
struct Args {
    /// Relay address (host:port)
    relay: String,

    /// Room name to observe
    #[arg(short, long)]
    room: String,

    /// Auth token for relay
    #[arg(long)]
    token: Option<String>,

    /// Identity seed (64-char hex)
    #[arg(long)]
    seed: Option<String>,

    /// Capture packets to file
    #[arg(long)]
    capture: Option<String>,

    /// Auto-stop after N seconds
    #[arg(long)]
    duration: Option<u64>,

    /// Disable TUI (print stats to stdout instead)
    #[arg(long)]
    no_tui: bool,
}

// ---------------------------------------------------------------------------
// Per-participant statistics
// ---------------------------------------------------------------------------

struct ParticipantStats {
    /// Stream identifier (index, assigned when we detect a new seq stream)
    stream_id: usize,
    /// Display name from RoomUpdate (if available)
    alias: Option<String>,
    /// Current codec
    codec: CodecId,
    /// Total packets received
    packets: u64,
    /// Detected lost packets (sequence gaps)
    lost: u64,
    /// Last seen sequence number
    last_seq: u16,
    /// Whether we've seen the first packet (for gap detection)
    seq_initialized: bool,
    /// EWMA jitter in ms
    jitter_ms: f64,
    /// Last packet arrival time
    last_arrival: Option<Instant>,
    /// Codec changes observed
    codec_switches: u32,
    /// First packet time
    first_seen: Instant,
    /// Last packet time
    last_seen: Instant,
}

impl ParticipantStats {
    fn new(id: usize, codec: CodecId) -> Self {
        let now = Instant::now();
        Self {
            stream_id: id,
            alias: None,
            codec,
            packets: 0,
            lost: 0,
            last_seq: 0,
            seq_initialized: false,
            jitter_ms: 0.0,
            last_arrival: None,
            codec_switches: 0,
            first_seen: now,
            last_seen: now,
        }
    }

    fn ingest(&mut self, pkt: &MediaPacket, now: Instant) {
        self.packets += 1;
        self.last_seen = now;

        // Codec switch detection
        if pkt.header.codec_id != self.codec {
            self.codec_switches += 1;
            self.codec = pkt.header.codec_id;
        }

        // Loss detection from sequence gaps
        if self.seq_initialized {
            let expected = self.last_seq.wrapping_add(1);
            let gap = pkt.header.seq.wrapping_sub(expected);
            if gap > 0 && gap < 100 {
                self.lost += gap as u64;
            }
        }
        self.last_seq = pkt.header.seq;
        self.seq_initialized = true;

        // Jitter (inter-arrival time variance, EWMA)
        if let Some(last) = self.last_arrival {
            let interval_ms = now.duration_since(last).as_secs_f64() * 1000.0;
            let expected_ms = pkt.header.codec_id.frame_duration_ms() as f64;
            let diff = (interval_ms - expected_ms).abs();
            self.jitter_ms = 0.1 * diff + 0.9 * self.jitter_ms;
        }
        self.last_arrival = Some(now);
    }

    fn loss_percent(&self) -> f64 {
        let total = self.packets + self.lost;
        if total == 0 {
            0.0
        } else {
            (self.lost as f64 / total as f64) * 100.0
        }
    }

    fn duration(&self) -> Duration {
        self.last_seen.duration_since(self.first_seen)
    }

    fn display_name(&self) -> String {
        self.alias
            .as_deref()
            .map(String::from)
            .unwrap_or_else(|| format!("Stream {}", self.stream_id))
    }
}

// ---------------------------------------------------------------------------
// Participant identification by sequence stream
// ---------------------------------------------------------------------------

/// Find the participant whose sequence counter is close to `seq`, or create a
/// new one.  Each sender has an independent wrapping u16 counter, so we can
/// distinguish streams by proximity of consecutive sequence numbers.
fn find_or_create_participant(
    participants: &mut Vec<ParticipantStats>,
    seq: u16,
    codec: CodecId,
) -> usize {
    for (i, p) in participants.iter().enumerate() {
        if p.seq_initialized {
            let delta = seq.wrapping_sub(p.last_seq);
            if delta > 0 && delta < 50 {
                return i;
            }
        }
    }
    // New stream detected
    let id = participants.len();
    participants.push(ParticipantStats::new(id, codec));
    id
}

// ---------------------------------------------------------------------------
// Capture writer (binary packet log for later replay)
// ---------------------------------------------------------------------------

struct CaptureWriter {
    file: std::io::BufWriter<std::fs::File>,
    start: Instant,
}

impl CaptureWriter {
    fn new(path: &str, room: &str, relay: &str) -> anyhow::Result<Self> {
        let file = std::fs::File::create(path)?;
        let mut writer = std::io::BufWriter::new(file);
        // Magic + version
        writer.write_all(b"WZP\x01")?;
        let header = serde_json::json!({
            "room": room,
            "relay": relay,
            "start_time": chrono::Utc::now().to_rfc3339(),
            "version": 1,
        });
        let header_bytes = serde_json::to_vec(&header)?;
        writer.write_all(&(header_bytes.len() as u32).to_le_bytes())?;
        writer.write_all(&header_bytes)?;
        Ok(Self {
            file: writer,
            start: Instant::now(),
        })
    }

    fn write_packet(&mut self, pkt: &MediaPacket, now: Instant) -> anyhow::Result<()> {
        let elapsed_us = now.duration_since(self.start).as_micros() as u64;
        self.file.write_all(&elapsed_us.to_le_bytes())?;
        let raw = pkt.to_bytes();
        self.file.write_all(&(raw.len() as u32).to_le_bytes())?;
        self.file.write_all(&raw)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// No-TUI mode (print stats to stdout periodically)
// ---------------------------------------------------------------------------

async fn run_no_tui(
    transport: &wzp_transport::QuinnTransport,
    participants: &mut Vec<ParticipantStats>,
    total_packets: &mut u64,
    deadline: Option<Instant>,
    mut capture_writer: Option<&mut CaptureWriter>,
) -> anyhow::Result<()> {
    let mut print_timer = Instant::now();
    loop {
        if let Some(dl) = deadline {
            if Instant::now() > dl {
                break;
            }
        }
        match tokio::time::timeout(Duration::from_millis(100), transport.recv_media()).await {
            Ok(Ok(Some(pkt))) => {
                let now = Instant::now();
                let idx =
                    find_or_create_participant(participants, pkt.header.seq, pkt.header.codec_id);
                participants[idx].ingest(&pkt, now);
                *total_packets += 1;
                if let Some(ref mut w) = capture_writer {
                    w.write_packet(&pkt, now)?;
                }
            }
            Ok(Ok(None)) => break,   // connection closed
            Ok(Err(e)) => {
                tracing::warn!("recv error: {e}");
                break;
            }
            Err(_) => {}              // timeout, loop again
        }
        if print_timer.elapsed() >= Duration::from_secs(2) {
            print_stats(participants, *total_packets);
            print_timer = Instant::now();
        }
    }
    Ok(())
}

fn print_stats(participants: &[ParticipantStats], total: u64) {
    eprintln!("--- {} participants | {} total packets ---", participants.len(), total);
    for p in participants {
        eprintln!(
            "  {}: {} pkts, {:.1}% loss, {:.0}ms jitter, {:?}, {:.0}s",
            p.display_name(),
            p.packets,
            p.loss_percent(),
            p.jitter_ms,
            p.codec,
            p.duration().as_secs_f64(),
        );
    }
}

// ---------------------------------------------------------------------------
// TUI mode (ratatui + crossterm)
// ---------------------------------------------------------------------------

async fn run_tui(
    transport: &wzp_transport::QuinnTransport,
    participants: &mut Vec<ParticipantStats>,
    total_packets: &mut u64,
    start_time: Instant,
    deadline: Option<Instant>,
    mut capture_writer: Option<&mut CaptureWriter>,
) -> anyhow::Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let mut redraw_timer = Instant::now();

    let result: anyhow::Result<()> = async {
        loop {
            // Check for quit key (q or Ctrl+C)
            if crossterm::event::poll(Duration::from_millis(0))? {
                if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                    use crossterm::event::{KeyCode, KeyModifiers};
                    if key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL))
                    {
                        break;
                    }
                }
            }

            if let Some(dl) = deadline {
                if Instant::now() > dl {
                    break;
                }
            }

            // Receive packets (non-blocking with short timeout)
            match tokio::time::timeout(Duration::from_millis(20), transport.recv_media()).await {
                Ok(Ok(Some(pkt))) => {
                    let now = Instant::now();
                    let idx = find_or_create_participant(
                        participants,
                        pkt.header.seq,
                        pkt.header.codec_id,
                    );
                    participants[idx].ingest(&pkt, now);
                    *total_packets += 1;
                    if let Some(ref mut w) = capture_writer {
                        w.write_packet(&pkt, now)?;
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => {
                    tracing::warn!("recv error: {e}");
                    break;
                }
                Err(_) => {}
            }

            // Redraw TUI at ~10 FPS
            if redraw_timer.elapsed() >= Duration::from_millis(100) {
                terminal.draw(|f| draw_ui(f, participants, *total_packets, start_time))?;
                redraw_timer = Instant::now();
            }
        }
        Ok(())
    }
    .await;

    // Always restore terminal, even on error
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen
    )?;

    result
}

fn draw_ui(
    f: &mut ratatui::Frame,
    participants: &[ParticipantStats],
    total_packets: u64,
    start_time: Instant,
) {
    use ratatui::layout::{Constraint, Direction, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};

    let elapsed = start_time.elapsed();
    let elapsed_str = format!(
        "{:02}:{:02}:{:02}",
        elapsed.as_secs() / 3600,
        (elapsed.as_secs() % 3600) / 60,
        elapsed.as_secs() % 60
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(5),   // participant table
            Constraint::Length(3), // footer
        ])
        .split(f.area());

    // Header
    let header = Paragraph::new(format!(
        " WZP Analyzer | {} participants | {} packets | {}",
        participants.len(),
        total_packets,
        elapsed_str
    ))
    .block(Block::default().borders(Borders::ALL).title(" Protocol Analyzer "));
    f.render_widget(header, chunks[0]);

    // Participant table
    let header_row = Row::new(vec![
        "#", "Name", "Codec", "Packets", "Loss%", "Jitter", "Switches", "Duration",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = participants
        .iter()
        .map(|p| {
            let loss_color = if p.loss_percent() > 5.0 {
                Color::Red
            } else if p.loss_percent() > 1.0 {
                Color::Yellow
            } else {
                Color::Green
            };

            Row::new(vec![
                format!("{}", p.stream_id),
                p.display_name(),
                format!("{:?}", p.codec),
                format!("{}", p.packets),
                format!("{:.1}%", p.loss_percent()),
                format!("{:.0}ms", p.jitter_ms),
                format!("{}", p.codec_switches),
                format!("{:.0}s", p.duration().as_secs_f64()),
            ])
            .style(Style::default().fg(loss_color))
        })
        .collect();

    let widths = [
        Constraint::Length(3),  // #
        Constraint::Length(20), // Name
        Constraint::Length(12), // Codec
        Constraint::Length(10), // Packets
        Constraint::Length(8),  // Loss%
        Constraint::Length(10), // Jitter
        Constraint::Length(10), // Switches
        Constraint::Length(10), // Duration
    ];

    let table = Table::new(rows, widths)
        .header(header_row)
        .block(Block::default().borders(Borders::ALL).title(" Participants "));
    f.render_widget(table, chunks[1]);

    // Footer
    let footer =
        Paragraph::new(" Press 'q' to quit ").block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

// ---------------------------------------------------------------------------
// Summary (printed on exit)
// ---------------------------------------------------------------------------

fn print_summary(participants: &[ParticipantStats], total: u64, elapsed: Duration) {
    eprintln!("\n=== Session Summary ===");
    eprintln!(
        "Duration: {:.1}s | Total packets: {} | Participants: {}",
        elapsed.as_secs_f64(),
        total,
        participants.len()
    );
    for p in participants {
        eprintln!(
            "  {}: {} pkts, {:.1}% loss, {:.0}ms jitter, {:?}, {} codec switches",
            p.display_name(),
            p.packets,
            p.loss_percent(),
            p.jitter_ms,
            p.codec,
            p.codec_switches,
        );
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Only init tracing subscriber in no-tui mode (it would corrupt the TUI otherwise)
    if args.no_tui {
        tracing_subscriber::fmt().init();
    }

    // TLS crypto provider
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Identity seed
    let seed = match &args.seed {
        Some(hex) => {
            let s = wzp_crypto::Seed::from_hex(hex).map_err(|e| anyhow::anyhow!(e))?;
            info!(fingerprint = %s.derive_identity().public_identity().fingerprint, "identity from --seed");
            s
        }
        None => {
            let s = wzp_crypto::Seed::generate();
            info!(fingerprint = %s.derive_identity().public_identity().fingerprint, "generated ephemeral identity");
            s
        }
    };

    // Connect to relay
    let relay_addr: std::net::SocketAddr = args.relay.parse()?;
    let bind_addr: std::net::SocketAddr = if relay_addr.is_ipv6() {
        "[::]:0".parse()?
    } else {
        "0.0.0.0:0".parse()?
    };
    let endpoint = wzp_transport::create_endpoint(bind_addr, None)?;
    let client_config = wzp_transport::client_config();
    let conn = wzp_transport::connect(&endpoint, relay_addr, &args.room, client_config).await?;
    let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));

    // Crypto handshake
    let _crypto_session =
        wzp_client::handshake::perform_handshake(&*transport, &seed.0, Some("analyzer")).await?;

    // Auth if token provided
    if let Some(ref token) = args.token {
        let auth = wzp_proto::SignalMessage::AuthToken {
            token: token.clone(),
        };
        transport.send_signal(&auth).await?;
    }

    // Capture file (optional)
    let mut capture_writer = args
        .capture
        .as_ref()
        .map(|path| CaptureWriter::new(path, &args.room, &args.relay))
        .transpose()?;

    // Duration timeout
    let deadline = args
        .duration
        .map(|s| Instant::now() + Duration::from_secs(s));

    // State
    let mut participants: Vec<ParticipantStats> = Vec::new();
    let mut total_packets: u64 = 0;
    let start_time = Instant::now();

    if args.no_tui {
        run_no_tui(
            &transport,
            &mut participants,
            &mut total_packets,
            deadline,
            capture_writer.as_mut(),
        )
        .await?;
    } else {
        run_tui(
            &transport,
            &mut participants,
            &mut total_packets,
            start_time,
            deadline,
            capture_writer.as_mut(),
        )
        .await?;
    }

    // Print summary
    print_summary(&participants, total_packets, start_time.elapsed());

    // Clean close
    transport.close().await?;

    Ok(())
}
