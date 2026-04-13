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
    /// Relay address (host:port) — required for live mode, ignored with --replay
    relay: Option<String>,

    /// Room name to observe — required for live mode, ignored with --replay
    #[arg(short, long)]
    room: Option<String>,

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

    /// Replay a captured .wzp file (offline analysis)
    #[arg(long)]
    replay: Option<String>,

    /// Generate HTML report (from live session or replay)
    #[arg(long)]
    html: Option<String>,

    /// Session key hex for decrypting payloads (enables audio decode)
    // TODO(#17): Audio decode requires session key + nonce context.
    // In SFU mode, payloads are E2E encrypted. Decoding requires
    // either: (a) session key from both endpoints, or (b) running
    // the analyzer as a trusted participant with its own key exchange.
    // For now, header-only analysis provides loss%, jitter, codec stats.
    #[arg(long)]
    key: Option<String>,
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
// Capture reader (for replay mode)
// ---------------------------------------------------------------------------

struct CaptureReader {
    reader: std::io::BufReader<std::fs::File>,
    header: serde_json::Value,
}

impl CaptureReader {
    fn open(path: &str) -> anyhow::Result<Self> {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        let mut reader = std::io::BufReader::new(file);

        // Read magic
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        anyhow::ensure!(&magic == b"WZP\x01", "not a WZP capture file");

        // Read header
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf)?;
        let header_len = u32::from_le_bytes(len_buf) as usize;
        let mut header_bytes = vec![0u8; header_len];
        reader.read_exact(&mut header_bytes)?;
        let header: serde_json::Value = serde_json::from_slice(&header_bytes)?;

        Ok(Self { reader, header })
    }

    fn next_packet(&mut self) -> anyhow::Result<Option<(u64, MediaPacket)>> {
        use std::io::Read;
        // Read timestamp
        let mut ts_buf = [0u8; 8];
        match self.reader.read_exact(&mut ts_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let timestamp_us = u64::from_le_bytes(ts_buf);

        // Read packet
        let mut len_buf = [0u8; 4];
        self.reader.read_exact(&mut len_buf)?;
        let pkt_len = u32::from_le_bytes(len_buf) as usize;
        let mut pkt_bytes = vec![0u8; pkt_len];
        self.reader.read_exact(&mut pkt_bytes)?;

        let pkt = MediaPacket::from_bytes(bytes::Bytes::from(pkt_bytes))
            .ok_or_else(|| anyhow::anyhow!("malformed packet in capture"))?;

        Ok(Some((timestamp_us, pkt)))
    }
}

// ---------------------------------------------------------------------------
// Timeline entry (for HTML report generation)
// ---------------------------------------------------------------------------

struct TimelineEntry {
    timestamp_us: u64,
    stream_id: usize,
    #[allow(dead_code)]
    codec: CodecId,
    #[allow(dead_code)]
    seq: u16,
    #[allow(dead_code)]
    payload_len: usize,
    loss_pct: f64,
    jitter_ms: f64,
}

// ---------------------------------------------------------------------------
// Replay mode (#15)
// ---------------------------------------------------------------------------

async fn run_replay(path: &str, args: &Args) -> anyhow::Result<()> {
    let mut reader = CaptureReader::open(path)?;
    eprintln!(
        "Replaying: {} (room: {})",
        path,
        reader
            .header
            .get("room")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
    );

    let mut participants: Vec<ParticipantStats> = Vec::new();
    let mut total_packets: u64 = 0;
    let start = Instant::now();
    let mut timeline: Vec<TimelineEntry> = Vec::new();

    while let Some((ts_us, pkt)) = reader.next_packet()? {
        let now = Instant::now();
        let idx = find_or_create_participant(&mut participants, pkt.header.seq, pkt.header.codec_id);
        participants[idx].ingest(&pkt, now);
        total_packets += 1;

        // Record for HTML timeline
        timeline.push(TimelineEntry {
            timestamp_us: ts_us,
            stream_id: idx,
            codec: pkt.header.codec_id,
            seq: pkt.header.seq,
            payload_len: pkt.payload.len(),
            loss_pct: participants[idx].loss_percent(),
            jitter_ms: participants[idx].jitter_ms,
        });
    }

    print_summary(&participants, total_packets, start.elapsed());

    // Generate HTML if requested
    if let Some(html_path) = &args.html {
        generate_html_report(html_path, &participants, &timeline, total_packets, &reader.header)?;
        eprintln!("HTML report: {}", html_path);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// HTML report generation (#16)
// ---------------------------------------------------------------------------

fn generate_html_report(
    path: &str,
    participants: &[ParticipantStats],
    timeline: &[TimelineEntry],
    total_packets: u64,
    capture_header: &serde_json::Value,
) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::File::create(path)?;

    let room = capture_header
        .get("room")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let start_time = capture_header
        .get("start_time")
        .and_then(|v| v.as_str())
        .unwrap_or("?");

    // Build per-stream loss/jitter timeline data for Chart.js
    // Sample every 1 second (group timeline entries by second)
    let max_ts = timeline.last().map(|e| e.timestamp_us).unwrap_or(0);
    let duration_secs = (max_ts / 1_000_000) + 1;

    let mut loss_data: std::collections::HashMap<usize, Vec<f64>> =
        std::collections::HashMap::new();
    let mut jitter_data: std::collections::HashMap<usize, Vec<f64>> =
        std::collections::HashMap::new();

    for stream_id in 0..participants.len() {
        loss_data.insert(stream_id, vec![0.0; duration_secs as usize]);
        jitter_data.insert(stream_id, vec![0.0; duration_secs as usize]);
    }

    for entry in timeline {
        let sec = (entry.timestamp_us / 1_000_000) as usize;
        if sec < duration_secs as usize {
            if let Some(losses) = loss_data.get_mut(&entry.stream_id) {
                losses[sec] = entry.loss_pct;
            }
            if let Some(jitters) = jitter_data.get_mut(&entry.stream_id) {
                jitters[sec] = entry.jitter_ms;
            }
        }
    }

    let colors = [
        "#e74c3c", "#3498db", "#2ecc71", "#f39c12", "#9b59b6", "#1abc9c",
    ];

    // Build dataset JSON for charts
    let mut loss_datasets = String::new();
    let mut jitter_datasets = String::new();
    for (i, p) in participants.iter().enumerate() {
        let name = p.display_name();
        let color = colors[i % colors.len()];
        let loss_vals = loss_data
            .get(&i)
            .map(|v| format!("{:?}", v))
            .unwrap_or_default();
        let jitter_vals = jitter_data
            .get(&i)
            .map(|v| format!("{:?}", v))
            .unwrap_or_default();

        loss_datasets.push_str(&format!(
            "{{ label: '{}', data: {}, borderColor: '{}', fill: false }},\n",
            name, loss_vals, color
        ));
        jitter_datasets.push_str(&format!(
            "{{ label: '{}', data: {}, borderColor: '{}', fill: false }},\n",
            name, jitter_vals, color
        ));
    }

    let labels: Vec<String> = (0..duration_secs).map(|s| format!("{}s", s)).collect();
    let labels_json = format!("{:?}", labels);

    // Summary table rows
    let mut summary_rows = String::new();
    for p in participants {
        summary_rows.push_str(&format!(
            "<tr><td>{}</td><td>{:?}</td><td>{}</td><td>{:.1}%</td><td>{:.0}ms</td><td>{}</td></tr>\n",
            p.display_name(),
            p.codec,
            p.packets,
            p.loss_percent(),
            p.jitter_ms,
            p.codec_switches
        ));
    }

    write!(
        f,
        r#"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<title>WZP Call Report — {room}</title>
<script src="https://cdn.jsdelivr.net/npm/chart.js@4"></script>
<style>
  body {{ font-family: -apple-system, sans-serif; max-width: 1200px; margin: 0 auto; padding: 20px; background: #1a1a2e; color: #e0e0e0; }}
  h1,h2 {{ color: #4a9eff; }}
  table {{ border-collapse: collapse; width: 100%; margin: 20px 0; }}
  th,td {{ border: 1px solid #333; padding: 8px 12px; text-align: left; }}
  th {{ background: #16213e; }}
  tr:nth-child(even) {{ background: #1a1a3e; }}
  .chart-container {{ background: #16213e; border-radius: 8px; padding: 16px; margin: 20px 0; }}
  canvas {{ max-height: 300px; }}
  .meta {{ color: #888; font-size: 0.9em; }}
</style>
</head><body>
<h1>WZP Call Quality Report</h1>
<p class="meta">Room: <b>{room}</b> | Start: {start_time} | Packets: {total_packets} | Duration: {duration_secs}s</p>

<h2>Participant Summary</h2>
<table>
<tr><th>Name</th><th>Codec</th><th>Packets</th><th>Loss</th><th>Jitter</th><th>Codec Switches</th></tr>
{summary_rows}
</table>

<h2>Packet Loss Over Time</h2>
<div class="chart-container"><canvas id="lossChart"></canvas></div>

<h2>Jitter Over Time</h2>
<div class="chart-container"><canvas id="jitterChart"></canvas></div>

<script>
const labels = {labels_json};
new Chart(document.getElementById('lossChart'), {{
  type: 'line',
  data: {{ labels, datasets: [{loss_datasets}] }},
  options: {{ responsive: true, scales: {{ y: {{ beginAtZero: true, title: {{ display: true, text: 'Loss %' }} }} }} }}
}});
new Chart(document.getElementById('jitterChart'), {{
  type: 'line',
  data: {{ labels, datasets: [{jitter_datasets}] }},
  options: {{ responsive: true, scales: {{ y: {{ beginAtZero: true, title: {{ display: true, text: 'Jitter (ms)' }} }} }} }}
}});
</script>
</body></html>"#
    )?;

    Ok(())
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
    if args.no_tui || args.replay.is_some() {
        tracing_subscriber::fmt().init();
    }

    if let Some(ref key) = args.key {
        eprintln!(
            "Note: --key provided ({} chars) but audio decode is not yet implemented.",
            key.len()
        );
        eprintln!("  Header-only analysis (loss%, jitter, codec stats) will proceed.");
    }

    // Replay mode: offline analysis of a .wzp capture file
    if let Some(ref replay_path) = args.replay {
        return run_replay(replay_path, &args).await;
    }

    // Live mode requires relay and room
    let relay = args
        .relay
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("relay address required for live mode (use --replay for offline)"))?;
    let room = args
        .room
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--room required for live mode (use --replay for offline)"))?;

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
    let relay_addr: std::net::SocketAddr = relay.parse()?;
    let bind_addr: std::net::SocketAddr = if relay_addr.is_ipv6() {
        "[::]:0".parse()?
    } else {
        "0.0.0.0:0".parse()?
    };
    let endpoint = wzp_transport::create_endpoint(bind_addr, None)?;
    let client_config = wzp_transport::client_config();
    let conn = wzp_transport::connect(&endpoint, relay_addr, room, client_config).await?;
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
        .map(|path| CaptureWriter::new(path, room, relay))
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
