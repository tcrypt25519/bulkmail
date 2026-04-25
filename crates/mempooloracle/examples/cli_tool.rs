#![allow(dead_code)]

use alloy::providers::{WebSocketConfig, WsConnect};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use mempooloracle::{
    Address, AlloyTrackerRuntime, AlloyTrackerTelemetry, AlloyTrackerTelemetrySnapshot,
    BlockNumber, BlockUpdate, ConsensusTransportConfig, ConsensusTransportImplementation,
    ExecutionHash, MempoolEvent, MempoolHandle, MempoolTracker, P2pBlockTransport,
    P2pTransportConfig, PeerEventSnapshot, PendingTx, Slot, TrackerConfig, TrackerTransport,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Bar, BarChart, BarGroup, Block, Borders, Gauge, List, ListItem, Paragraph, Wrap},
};
use std::{
    env, io,
    path::PathBuf,
    process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[tokio::main]
async fn main() -> io::Result<()> {
    // Initialize tracing for verbose logging
    tracing_subscriber::fmt::init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = parse_args();
    if let Some(port) = args.metrics_port {
        mempooloracle::init_metrics(port).expect("failed to initialize metrics");
    }
    let stop = Arc::new(AtomicBool::new(false));
    let config = TrackerConfig {
        initial_base_fee: 10,
        global_capacity: 1000,
        per_account_capacity: 100,
        private_flow_prior: 0.1,
    };

    let (runtime, telemetry) = match args.source {
        DataSource::Live { ws_url } => {
            let transport = TrackerTransport::Rpc(mempooloracle::RpcTransportConfig {
                ws: WsConnect::new(ws_url).with_config(
                    WebSocketConfig::default()
                        .max_message_size(Some(128 << 20))
                        .max_frame_size(Some(128 << 20)),
                ),
            });
            let runtime = MempoolTracker::connect(transport, config.clone())
                .await
                .map_err(|err| {
                    io::Error::other(format!("failed to connect mempool oracle: {err}"))
                })?;

            let telemetry = TelemetrySource::Live(runtime.telemetry());
            (Runtime::Live(runtime), telemetry)
        }
        DataSource::P2p {
            chain,
            bootnodes,
            discovery_v4,
            log_path,
            execution_port,
            consensus_port,
        } => {
            let transport = TrackerTransport::P2p(P2pTransportConfig {
                chain,
                bootnodes,
                discovery_v4,
                listen_addr: None,
                execution_port,
                consensus_port,
                block_transport: P2pBlockTransport::Consensus(ConsensusTransportConfig {
                    implementation: ConsensusTransportImplementation::Eth2Libp2p,
                }),
                log_path,
            });
            let runtime = MempoolTracker::connect(transport, config.clone())
                .await
                .map_err(|err| {
                    io::Error::other(format!(
                        "failed to connect mempool oracle p2p transport: {err}"
                    ))
                })?;

            let telemetry = TelemetrySource::Live(runtime.telemetry());
            (Runtime::Live(runtime), telemetry)
        }
        DataSource::Simulated => {
            let telemetry = Arc::new(Mutex::new(Telemetry::default()));
            let (event_sender, event_receiver) = mpsc::channel();
            let (handle, tracker) = MempoolTracker::new(event_receiver, &[], config);
            let tracker_thread = thread::spawn(move || tracker.run());

            let simulation_stop = stop.clone();
            let telemetry_clone = telemetry.clone();
            let simulation_thread = thread::spawn(move || {
                simulate_blockchain(event_sender, simulation_stop, telemetry_clone)
            });

            (
                Runtime::Simulated {
                    handle,
                    simulation_thread,
                    tracker_thread,
                },
                TelemetrySource::Simulated(telemetry),
            )
        }
    };

    let handle = runtime.handle();

    let result = run_plaintext(
        handle,
        args.duration_secs,
        stop.clone(),
        telemetry,
        args.ttfpt,
    );

    stop.store(true, Ordering::Relaxed);
    runtime.shutdown();

    result
}

enum Runtime {
    Live(AlloyTrackerRuntime),
    Simulated {
        handle: MempoolHandle,
        simulation_thread: thread::JoinHandle<()>,
        tracker_thread: thread::JoinHandle<()>,
    },
}

impl Runtime {
    fn handle(&self) -> MempoolHandle {
        match self {
            Self::Live(runtime) => runtime.handle(),
            Self::Simulated { handle, .. } => handle.clone(),
        }
    }

    fn shutdown(self) {
        match self {
            Self::Live(runtime) => drop(runtime),
            Self::Simulated {
                simulation_thread,
                tracker_thread,
                ..
            } => {
                let _ = simulation_thread.join();
                let _ = tracker_thread.join();
            }
        }
    }
}

struct CliArgs {
    duration_secs: u64,
    source: DataSource,
    metrics_port: Option<u16>,
    ttfpt: bool,
}

enum DataSource {
    Live {
        ws_url: String,
    },
    P2p {
        chain: String,
        bootnodes: Vec<String>,
        discovery_v4: bool,
        log_path: Option<PathBuf>,
        execution_port: Option<u16>,
        consensus_port: Option<u16>,
    },
    Simulated,
}

enum TelemetrySource {
    Live(AlloyTrackerTelemetry),
    Simulated(Arc<Mutex<Telemetry>>),
}

impl TelemetrySource {
    fn snapshot(&self) -> AlloyTelemetryView {
        match self {
            Self::Live(telemetry) => AlloyTelemetryView::from(telemetry.snapshot()),
            Self::Simulated(telemetry) => {
                AlloyTelemetryView::from(telemetry.lock().expect("telemetry lock poisoned").clone())
            }
        }
    }
}

fn run_app(
    mut terminal: DefaultTerminal,
    handle: MempoolHandle,
    duration_secs: u64,
    stop: Arc<AtomicBool>,
    telemetry: TelemetrySource,
) -> io::Result<()> {
    let started_at = Instant::now();
    let deadline = (duration_secs > 0).then(|| started_at + Duration::from_secs(duration_secs));
    let mut app = App::new(duration_secs);

    while !stop.load(Ordering::Relaxed) {
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            break;
        }

        let snapshot = Snapshot::capture(&handle, &telemetry, started_at);
        app.push_snapshot(snapshot);
        terminal.draw(|frame| render(frame, &mut app))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            let terminal_size = terminal.size()?;
            let list_rows =
                transaction_pane_rows(Rect::new(0, 0, terminal_size.width, terminal_size.height));

            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                break;
            }

            match key.code {
                KeyCode::Up | KeyCode::Char('k') => app.scroll_up(1),
                KeyCode::Down | KeyCode::Char('j') => app.scroll_down(1, list_rows),
                KeyCode::PageUp => app.scroll_up(list_rows.max(1)),
                KeyCode::PageDown => app.scroll_down(list_rows.max(1), list_rows),
                _ => {}
            }
        }
    }

    Ok(())
}

fn run_plaintext(
    handle: MempoolHandle,
    duration_secs: u64,
    stop: Arc<AtomicBool>,
    telemetry: TelemetrySource,
    ttfpt: bool,
) -> io::Result<()> {
    let started_at = Instant::now();
    let deadline = (duration_secs > 0).then(|| started_at + Duration::from_secs(duration_secs));
    let mut last_render = Instant::now() - Duration::from_secs(1);

    while !stop.load(Ordering::Relaxed) {
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            break;
        }

        if last_render.elapsed() >= Duration::from_millis(500) {
            let snapshot = Snapshot::capture(&handle, &telemetry, started_at);

            if ttfpt && snapshot.pending_seen > 0 {
                println!("\nSUCCESS: First pending transaction observed!");
                println!(
                    "Time to first pending transaction: {:?}",
                    started_at.elapsed()
                );
                process::exit(0);
            }

            let unfinalized_depth = snapshot
                .last_block_number
                .0
                .saturating_sub(snapshot.consensus_finalized_number.0);
            println!(
                "base_fee={} expected_txs={} depth={} blocks={} pending_seen={} finalized_block={} peers(el={}, cl={})",
                snapshot.base_fee,
                snapshot.expected_txs,
                unfinalized_depth,
                snapshot.block_count,
                snapshot.pending_seen,
                snapshot.consensus_finalized_number.0,
                snapshot.p2p_peer_count,
                snapshot.consensus_peer_count,
            );
            println!(
                "  EL: txs_in={} hashes_in={} unique(in={}, out={}) conns(in={}, out={}) active(in={}, out={})",
                snapshot.el_txs_received,
                snapshot.el_tx_hashes_received,
                snapshot.el_unique_peers_ingress,
                snapshot.el_unique_peers_egress,
                snapshot.el_connections_ingress,
                snapshot.el_connections_egress,
                snapshot.el_active_connections_ingress,
                snapshot.el_active_connections_egress,
            );
            println!(
                "  CL: blocks_in={} finality_in={} unique(in={}, out={}) range_req(sent={}, resp={}, lat={}ms) status(in={}, out={})",
                snapshot.cl_blocks_received,
                snapshot.cl_finality_updates_received,
                snapshot.cl_unique_peers_ingress,
                snapshot.cl_unique_peers_egress,
                snapshot.cl_blocks_by_range_requests_sent,
                snapshot.cl_blocks_by_range_responses_received,
                snapshot.cl_blocks_by_range_latency_avg_ns / 1_000_000,
                snapshot.cl_status_received,
                snapshot.cl_status_sent,
            );
            if snapshot.recent_peer_events.is_empty() {
                println!("  peers: no peer events captured yet");
            } else {
                println!("  recent peer events:");
                for event in snapshot.recent_peer_events.iter().rev().take(4).rev() {
                    println!("    {}", format_peer_event(event));
                }
            }

            last_render = Instant::now();
        }

        thread::sleep(Duration::from_millis(100));
    }

    Ok(())
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Min(12),
            Constraint::Length(3),
        ])
        .split(area);

    render_header(frame, outer[0], app);
    render_gauges(frame, outer[1], app);
    render_body(frame, outer[2], app);
    render_footer(frame, outer[3], app);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let snapshot = app.latest();
    let mode = if app.duration_secs == 0 {
        "live until Ctrl-C"
    } else {
        "timed capture"
    };
    let text = Line::from(vec![
        Span::styled(
            "Mempool Oracle",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(mode, Style::default().fg(Color::Yellow)),
        Span::raw("  "),
        Span::styled(
            format!("uptime {}s", snapshot.elapsed_secs),
            Style::default().fg(Color::Gray),
        ),
        Span::raw("  "),
        Span::styled(
            format!("blocks {}", snapshot.block_count),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  "),
        Span::styled(
            format!("pending seen {}", snapshot.pending_seen),
            Style::default().fg(Color::Magenta),
        ),
    ]);

    let header =
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("Overview"));
    frame.render_widget(header, area);
}

fn render_gauges(frame: &mut Frame, area: Rect, app: &App) {
    let snapshot = app.latest();
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    let private_flow = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title("Private Flow"))
        .gauge_style(Style::default().fg(Color::LightYellow))
        .percent(snapshot.private_flow_percent.min(100) as u16)
        .label(format!("{:.1}%", snapshot.private_flow));
    frame.render_widget(private_flow, chunks[0]);

    let next_block_fill = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Next Block Fill"),
        )
        .gauge_style(Style::default().fg(Color::LightGreen))
        .percent(snapshot.next_block_fill_percent.min(100) as u16)
        .label(format!("{:.1}%", snapshot.next_block_fill_percent));
    frame.render_widget(next_block_fill, chunks[1]);

    let marketable = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Marketable Set"),
        )
        .gauge_style(Style::default().fg(Color::LightBlue))
        .percent(snapshot.marketable_utilization.min(100) as u16)
        .label(format!("{} tracked", snapshot.marketable_count));
    frame.render_widget(marketable, chunks[2]);

    let unfinalized_depth = snapshot
        .last_block_number
        .0
        .saturating_sub(snapshot.consensus_finalized_number.0);
    let depth_percent = (unfinalized_depth.min(128) as f64 / 128.0 * 100.0) as u16;
    let depth_gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Unfinalized Depth"),
        )
        .gauge_style(Style::default().fg(Color::LightRed))
        .percent(depth_percent)
        .label(format!("{}", unfinalized_depth));
    frame.render_widget(depth_gauge, chunks[3]);
}

fn render_body(frame: &mut Frame, area: Rect, app: &mut App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(25),
            Constraint::Percentage(45),
        ])
        .split(area);

    render_left_column(frame, columns[0], app);
    render_base_fee_chart(frame, columns[1], app);
    render_top_transactions(frame, columns[2], app);
}

fn render_left_column(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Min(5)])
        .split(area);
    render_metrics(frame, rows[0], app);
    render_peer_events(frame, rows[1], app);
}

fn render_metrics(frame: &mut Frame, area: Rect, app: &App) {
    let snapshot = app.latest();
    let lines = vec![
        Line::from(vec![
            "Base fee: ".into(),
            format!("{}", snapshot.base_fee).cyan().bold(),
        ]),
        Line::from(vec![
            "Min tip next block: ".into(),
            format!("{}", snapshot.min_tip).green().bold(),
        ]),
        Line::from(vec![
            "Expected txs next block: ".into(),
            format!("{}", snapshot.expected_txs).yellow().bold(),
        ]),
        Line::from(vec![
            "Gas in next block set: ".into(),
            format!("{}", snapshot.gas_used_for_next_block).into(),
        ]),
        Line::from(vec![
            "Usable capacity: ".into(),
            format!("{}", snapshot.usable_capacity).into(),
        ]),
        Line::from(vec![
            "Last block gas used: ".into(),
            format!("{}", snapshot.last_block_gas_used).into(),
        ]),
        Line::from(vec![
            "Last block txs: ".into(),
            format!("{}", snapshot.last_block_txs).into(),
        ]),
        Line::from(vec![
            "Tracker queue depth: ".into(),
            format!("{}", snapshot.marketable_count).into(),
        ]),
    ];

    let metrics = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Live Metrics"))
        .wrap(Wrap { trim: true });
    frame.render_widget(metrics, area);
}

fn render_peer_events(frame: &mut Frame, area: Rect, app: &App) {
    let visible_rows = area.height.saturating_sub(2).max(1) as usize;
    let items: Vec<ListItem> = if app.latest().recent_peer_events.is_empty() {
        vec![ListItem::new("No peer events captured yet.")]
    } else {
        app.latest()
            .recent_peer_events
            .iter()
            .rev()
            .take(visible_rows)
            .map(|event| ListItem::new(format_peer_event(event)))
            .collect()
    };

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Recent Peer Events"),
    );
    frame.render_widget(list, area);
}

fn format_peer_event(event: &PeerEventSnapshot) -> String {
    let peer = short_peer_id(&event.peer_id);
    if event.detail.is_empty() {
        format!(
            "{} {:>3} {:>3} {:<22} {}",
            event.timestamp_ms, event.layer, event.direction, event.event, peer
        )
    } else {
        format!(
            "{} {:>3} {:>3} {:<22} {} {}",
            event.timestamp_ms,
            event.layer,
            event.direction,
            event.event,
            peer,
            truncate(&event.detail, 72)
        )
    }
}

fn short_peer_id(peer_id: &str) -> &str {
    peer_id.get(..12).unwrap_or(peer_id)
}

fn truncate(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        value.to_owned()
    } else {
        let prefix: String = value.chars().take(max_len.saturating_sub(3)).collect();
        format!("{prefix}...")
    }
}

fn render_base_fee_chart(frame: &mut Frame, area: Rect, app: &App) {
    let bars: Vec<Bar> = app
        .snapshots
        .iter()
        .rev()
        .take(10)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .enumerate()
        .map(|(index, snapshot)| {
            Bar::default()
                .value(snapshot.base_fee.min(u64::MAX as u128) as u64)
                .label(format!("{}", index + 1).into())
                .style(Style::default().fg(Color::Cyan))
        })
        .collect();

    let group = BarGroup::default().bars(&bars);
    let chart = BarChart::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Recent Base Fee"),
        )
        .data(group)
        .bar_width(4)
        .bar_gap(1)
        .value_style(Style::default().fg(Color::Black).bg(Color::Cyan));

    frame.render_widget(chart, area);
}

fn render_top_transactions(frame: &mut Frame, area: Rect, app: &mut App) {
    let visible_rows = transaction_rows(app.latest(), area.height);
    app.clamp_scroll(visible_rows);

    let title = if app.latest().ranked_rows.is_empty() {
        format!(
            "Ranked Pending Transactions [0 of {}]",
            app.latest().marketable_count
        )
    } else if app.latest().marketable_count > app.latest().ranked_rows.len() {
        format!(
            "Ranked Pending Transactions [{}-{} of {} | sampled top {}]",
            app.tx_scroll + 1,
            (app.tx_scroll + visible_rows).min(app.latest().ranked_rows.len()),
            app.latest().marketable_count,
            app.latest().ranked_rows.len()
        )
    } else {
        format!(
            "Ranked Pending Transactions [{}-{} of {}]",
            app.tx_scroll + 1,
            (app.tx_scroll + visible_rows).min(app.latest().ranked_rows.len()),
            app.latest().marketable_count
        )
    };

    let items: Vec<ListItem> = if app.latest().ranked_rows.is_empty() {
        vec![ListItem::new("No marketable transactions yet.")]
    } else {
        app.latest()
            .ranked_rows
            .iter()
            .skip(app.tx_scroll)
            .take(visible_rows)
            .map(|row| {
                ListItem::new(Line::from(vec![
                    format!("{:>4} ", row.nonce).yellow(),
                    format!("eff {:>3} ", row.effective_priority_fee).cyan(),
                    format!("tip {:>3} ", row.priority_fee).green(),
                    format!("max {:>3} ", row.max_fee).magenta(),
                    format!("gas {:>6} ", row.gas_limit).into(),
                    format!("eta {:>2} ", row.eta_blocks).blue(),
                    format!("{:02x}{:02x}..", row.sender_prefix.0, row.sender_prefix.1).dark_gray(),
                ]))
            })
            .collect()
    };

    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(list, area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let text = if app.duration_secs == 0 {
        "Up/Down or j/k scroll txs. PgUp/PgDn jump. Ctrl-C, q, or Esc quits."
    } else {
        "Timed run active. Up/Down or j/k scroll txs. Ctrl-C, q, or Esc exits early."
    };
    let footer = Paragraph::new(text)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL).title("Controls"));
    frame.render_widget(footer, area);
}

fn transaction_rows(snapshot: &Snapshot, area_height: u16) -> usize {
    if snapshot.ranked_rows.is_empty() {
        1
    } else {
        area_height.saturating_sub(2).max(1) as usize
    }
}

fn transaction_pane_rows(area: Rect) -> usize {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Min(12),
            Constraint::Length(3),
        ])
        .split(area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(25),
            Constraint::Percentage(45),
        ])
        .split(outer[2]);

    columns[2].height.saturating_sub(2).max(1) as usize
}

struct App {
    duration_secs: u64,
    snapshots: Vec<Snapshot>,
    tx_scroll: usize,
}

impl App {
    fn new(duration_secs: u64) -> Self {
        Self {
            duration_secs,
            snapshots: Vec::new(),
            tx_scroll: 0,
        }
    }

    fn push_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshots.push(snapshot);
        if self.snapshots.len() > 60 {
            let overflow = self.snapshots.len() - 60;
            self.snapshots.drain(0..overflow);
        }

        self.clamp_scroll(0);
    }

    fn latest(&self) -> &Snapshot {
        self.snapshots
            .last()
            .expect("app should always contain at least one snapshot")
    }

    fn scroll_up(&mut self, amount: usize) {
        self.tx_scroll = self.tx_scroll.saturating_sub(amount);
    }

    fn scroll_down(&mut self, amount: usize, visible_rows: usize) {
        let max_scroll = self.max_scroll(visible_rows);
        self.tx_scroll = (self.tx_scroll + amount).min(max_scroll);
    }

    fn clamp_scroll(&mut self, visible_rows: usize) {
        self.tx_scroll = self.tx_scroll.min(self.max_scroll(visible_rows));
    }

    fn max_scroll(&self, visible_rows: usize) -> usize {
        self.latest()
            .ranked_rows
            .len()
            .saturating_sub(visible_rows.max(1))
    }
}

#[derive(Clone)]
struct Snapshot {
    elapsed_secs: u64,
    base_fee: u128,
    private_flow: f64,
    private_flow_percent: u64,
    min_tip: u128,
    expected_txs: usize,
    gas_used_for_next_block: u64,
    usable_capacity: u64,
    next_block_fill_percent: u64,
    marketable_count: usize,
    marketable_utilization: u64,
    block_count: u64,
    last_block_number: BlockNumber,
    pending_seen: u64,
    last_block_txs: usize,
    last_block_gas_used: u64,
    consensus_finalized_slot: Slot,
    consensus_finalized_number: BlockNumber,
    p2p_peer_count: usize,
    consensus_peer_count: usize,

    // Peer metrics
    el_connections_ingress: u64,
    el_connections_egress: u64,
    cl_connections_ingress: u64,
    cl_connections_egress: u64,
    el_unique_peers_ingress: u64,
    el_unique_peers_egress: u64,
    cl_unique_peers_ingress: u64,
    cl_unique_peers_egress: u64,
    el_active_connections_ingress: u64,
    el_active_connections_egress: u64,
    cl_active_connections_ingress: u64,
    cl_active_connections_egress: u64,

    el_tx_hashes_received: u64,
    el_txs_received: u64,
    cl_blocks_received: u64,
    cl_finality_updates_received: u64,
    cl_status_received: u64,
    cl_status_sent: u64,
    cl_blocks_by_range_requests_sent: u64,
    cl_blocks_by_range_responses_received: u64,
    cl_blocks_by_range_latency_avg_ns: u64,
    recent_peer_events: Vec<PeerEventSnapshot>,
    ranked_rows: Vec<TxRow>,
}

impl Snapshot {
    fn capture(handle: &MempoolHandle, telemetry: &TelemetrySource, started_at: Instant) -> Self {
        let base_fee = handle.current_base_fee();
        let private_flow = handle.private_flow_ratio() * 100.0;
        let last_block_gas_limit = handle.last_block_gas_limit();
        let usable_capacity =
            (last_block_gas_limit as f64 * (1.0 - handle.private_flow_ratio())) as u64;
        let pq = handle.priority_queue();

        let mut expected_txs = 0usize;
        let mut min_tip = 0u128;
        let mut gas_used = 0u64;
        let mut ranked_rows = Vec::new();

        for (fee, addr, nonce) in &pq {
            if let Some(tx) = handle.find_tx_by_addr_and_nonce(*addr, *nonce) {
                if ranked_rows.len() < 256 {
                    let eta_blocks = handle.estimated_blocks_to_confirm(&tx.id).unwrap_or(0);
                    ranked_rows.push(TxRow {
                        nonce: tx.nonce,
                        effective_priority_fee: *fee,
                        priority_fee: tx.max_priority_fee_per_gas,
                        max_fee: tx.max_fee_per_gas,
                        gas_limit: tx.gas_limit,
                        eta_blocks,
                        sender_prefix: (tx.sender.0[0], tx.sender.0[1]),
                    });
                }

                if gas_used + tx.gas_limit > usable_capacity {
                    continue;
                }

                gas_used += tx.gas_limit;
                min_tip = *fee;
                expected_txs += 1;
            }
        }

        let next_block_fill_percent = if usable_capacity == 0 {
            0
        } else {
            gas_used.saturating_mul(100) / usable_capacity
        };

        let telemetry = telemetry.snapshot();

        Self {
            elapsed_secs: started_at.elapsed().as_secs(),
            base_fee,
            private_flow,
            private_flow_percent: private_flow.round() as u64,
            min_tip,
            expected_txs,
            gas_used_for_next_block: gas_used,
            usable_capacity,
            next_block_fill_percent,
            marketable_count: pq.len(),
            marketable_utilization: (pq.len().min(1000) as u64 * 100) / 1000,
            block_count: telemetry.block_count,
            last_block_number: telemetry.last_block_number,
            pending_seen: telemetry.pending_seen,
            last_block_txs: telemetry.last_block_txs,
            last_block_gas_used: telemetry.last_block_gas_used,
            consensus_finalized_slot: telemetry.consensus_finalized_slot,
            consensus_finalized_number: telemetry.consensus_finalized_number,
            p2p_peer_count: telemetry.p2p_peer_count,
            consensus_peer_count: telemetry.consensus_peer_count,

            el_connections_ingress: telemetry.el_connections_ingress,
            el_connections_egress: telemetry.el_connections_egress,
            cl_connections_ingress: telemetry.cl_connections_ingress,
            cl_connections_egress: telemetry.cl_connections_egress,
            el_unique_peers_ingress: telemetry.el_unique_peers_ingress,
            el_unique_peers_egress: telemetry.el_unique_peers_egress,
            cl_unique_peers_ingress: telemetry.cl_unique_peers_ingress,
            cl_unique_peers_egress: telemetry.cl_unique_peers_egress,
            el_active_connections_ingress: telemetry.el_active_connections_ingress,
            el_active_connections_egress: telemetry.el_active_connections_egress,
            cl_active_connections_ingress: telemetry.cl_active_connections_ingress,
            cl_active_connections_egress: telemetry.cl_active_connections_egress,

            el_tx_hashes_received: telemetry.el_tx_hashes_received,
            el_txs_received: telemetry.el_txs_received,
            cl_blocks_received: telemetry.cl_blocks_received,
            cl_finality_updates_received: telemetry.cl_finality_updates_received,
            cl_status_received: telemetry.cl_status_received,
            cl_status_sent: telemetry.cl_status_sent,
            cl_blocks_by_range_requests_sent: telemetry.cl_blocks_by_range_requests_sent,
            cl_blocks_by_range_responses_received: telemetry.cl_blocks_by_range_responses_received,
            cl_blocks_by_range_latency_avg_ns: telemetry.cl_blocks_by_range_latency_avg_ns,
            recent_peer_events: telemetry.recent_peer_events,
            ranked_rows,
        }
    }
}

#[derive(Clone)]
struct TxRow {
    nonce: u64,
    effective_priority_fee: u128,
    priority_fee: u128,
    max_fee: u128,
    gas_limit: u64,
    eta_blocks: u64,
    sender_prefix: (u8, u8),
}

#[derive(Clone, Default)]
struct Telemetry {
    pending_seen: u64,
    block_count: u64,
    last_block_number: BlockNumber,
    last_block_txs: usize,
    last_block_gas_used: u64,
    consensus_finalized_slot: Slot,
    consensus_finalized_number: BlockNumber,
    p2p_peer_count: usize,
    consensus_peer_count: usize,

    // Peer metrics
    el_connections_ingress: u64,
    el_connections_egress: u64,
    cl_connections_ingress: u64,
    cl_connections_egress: u64,
    el_unique_peers_ingress: u64,
    el_unique_peers_egress: u64,
    cl_unique_peers_ingress: u64,
    cl_unique_peers_egress: u64,
    el_active_connections_ingress: u64,
    el_active_connections_egress: u64,
    cl_active_connections_ingress: u64,
    cl_active_connections_egress: u64,

    el_tx_hashes_received: u64,
    el_txs_received: u64,
    cl_blocks_received: u64,
    cl_finality_updates_received: u64,
    cl_status_received: u64,
    cl_status_sent: u64,
    cl_blocks_by_range_requests_sent: u64,
    cl_blocks_by_range_responses_received: u64,
    cl_blocks_by_range_latency_avg_ns: u64,
    recent_peer_events: Vec<PeerEventSnapshot>,
}

#[derive(Clone, Default)]
struct AlloyTelemetryView {
    pending_seen: u64,
    block_count: u64,
    last_block_number: BlockNumber,
    last_block_txs: usize,
    last_block_gas_used: u64,
    consensus_finalized_slot: Slot,
    consensus_finalized_number: BlockNumber,
    p2p_peer_count: usize,
    consensus_peer_count: usize,

    // Peer metrics
    el_connections_ingress: u64,
    el_connections_egress: u64,
    cl_connections_ingress: u64,
    cl_connections_egress: u64,
    el_unique_peers_ingress: u64,
    el_unique_peers_egress: u64,
    cl_unique_peers_ingress: u64,
    cl_unique_peers_egress: u64,
    el_active_connections_ingress: u64,
    el_active_connections_egress: u64,
    cl_active_connections_ingress: u64,
    cl_active_connections_egress: u64,

    el_tx_hashes_received: u64,
    el_txs_received: u64,
    cl_blocks_received: u64,
    cl_finality_updates_received: u64,
    cl_status_received: u64,
    cl_status_sent: u64,
    cl_blocks_by_range_requests_sent: u64,
    cl_blocks_by_range_responses_received: u64,
    cl_blocks_by_range_latency_avg_ns: u64,
    recent_peer_events: Vec<PeerEventSnapshot>,
}

impl From<Telemetry> for AlloyTelemetryView {
    fn from(value: Telemetry) -> Self {
        Self {
            pending_seen: value.pending_seen,
            block_count: value.block_count,
            last_block_number: value.last_block_number,
            last_block_txs: value.last_block_txs,
            last_block_gas_used: value.last_block_gas_used,
            consensus_finalized_slot: value.consensus_finalized_slot,
            consensus_finalized_number: value.consensus_finalized_number,
            p2p_peer_count: value.p2p_peer_count,
            consensus_peer_count: value.consensus_peer_count,
            el_connections_ingress: value.el_connections_ingress,
            el_connections_egress: value.el_connections_egress,
            cl_connections_ingress: value.cl_connections_ingress,
            cl_connections_egress: value.cl_connections_egress,
            el_unique_peers_ingress: value.el_unique_peers_ingress,
            el_unique_peers_egress: value.el_unique_peers_egress,
            cl_unique_peers_ingress: value.cl_unique_peers_ingress,
            cl_unique_peers_egress: value.cl_unique_peers_egress,
            el_active_connections_ingress: value.el_active_connections_ingress,
            el_active_connections_egress: value.el_active_connections_egress,
            cl_active_connections_ingress: value.cl_active_connections_ingress,
            cl_active_connections_egress: value.cl_active_connections_egress,
            el_tx_hashes_received: value.el_tx_hashes_received,
            el_txs_received: value.el_txs_received,
            cl_blocks_received: value.cl_blocks_received,
            cl_finality_updates_received: value.cl_finality_updates_received,
            cl_status_received: value.cl_status_received,
            cl_status_sent: value.cl_status_sent,
            cl_blocks_by_range_requests_sent: value.cl_blocks_by_range_requests_sent,
            cl_blocks_by_range_responses_received: value.cl_blocks_by_range_responses_received,
            cl_blocks_by_range_latency_avg_ns: value.cl_blocks_by_range_latency_avg_ns,
            recent_peer_events: Vec::new(),
        }
    }
}

impl From<AlloyTrackerTelemetrySnapshot> for AlloyTelemetryView {
    fn from(value: AlloyTrackerTelemetrySnapshot) -> Self {
        Self {
            pending_seen: value.pending_seen,
            block_count: value.block_count,
            last_block_number: value.consensus_last_block_number,
            last_block_txs: value.last_block_txs,
            last_block_gas_used: value.last_block_gas_used,
            consensus_finalized_slot: value.consensus_finalized_slot,
            consensus_finalized_number: value.consensus_finalized_number,
            p2p_peer_count: value.p2p_peer_count,
            consensus_peer_count: value.consensus_peer_count,
            el_connections_ingress: value.el_connections_ingress,
            el_connections_egress: value.el_connections_egress,
            cl_connections_ingress: value.cl_connections_ingress,
            cl_connections_egress: value.cl_connections_egress,
            el_unique_peers_ingress: value.el_unique_peers_ingress,
            el_unique_peers_egress: value.el_unique_peers_egress,
            cl_unique_peers_ingress: value.cl_unique_peers_ingress,
            cl_unique_peers_egress: value.cl_unique_peers_egress,
            el_active_connections_ingress: value.el_active_connections_ingress,
            el_active_connections_egress: value.el_active_connections_egress,
            cl_active_connections_ingress: value.cl_active_connections_ingress,
            cl_active_connections_egress: value.cl_active_connections_egress,
            el_tx_hashes_received: value.el_tx_hashes_received,
            el_txs_received: value.el_txs_received,
            cl_blocks_received: value.cl_blocks_received,
            cl_finality_updates_received: value.cl_finality_updates_received,
            cl_status_received: value.cl_status_received,
            cl_status_sent: value.cl_status_sent,
            cl_blocks_by_range_requests_sent: value.cl_blocks_by_range_requests_sent,
            cl_blocks_by_range_responses_received: value.cl_blocks_by_range_responses_received,
            cl_blocks_by_range_latency_avg_ns: value.cl_blocks_by_range_latency_avg_ns,
            recent_peer_events: value.recent_peer_events.into_iter().collect(),
        }
    }
}

fn parse_args() -> CliArgs {
    let mut args = env::args().skip(1);
    let mut duration_secs = 30_u64;
    let mut ws_url = env::var("MEMPOOLORACLE_WS_URL")
        .ok()
        .or_else(|| env::var("ETH_WS_URL").ok());
    let mut simulate = false;
    let mut p2p = false;
    let mut chain = String::from("mainnet");
    let mut bootnodes = Vec::new();
    let mut discovery_v4 = true;
    let mut p2p_log_path = None;
    let mut execution_port = None;
    let mut consensus_port = None;
    let mut metrics_port = None;
    let mut ttfpt = false;

    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--duration=") {
            duration_secs = parse_duration_value(value);
            continue;
        }

        if arg == "--duration" {
            let Some(value) = args.next() else {
                eprintln!("missing value for --duration");
                print_usage_and_exit();
            };
            duration_secs = parse_duration_value(&value);
            continue;
        }

        if let Some(value) = arg.strip_prefix("--ws-url=") {
            ws_url = Some(value.to_owned());
            continue;
        }

        if arg == "--ws-url" {
            let Some(value) = args.next() else {
                eprintln!("missing value for --ws-url");
                print_usage_and_exit();
            };
            ws_url = Some(value);
            continue;
        }

        if arg == "--simulate" {
            simulate = true;
            continue;
        }

        if arg == "--p2p" {
            p2p = true;
            continue;
        }

        if arg == "--ttfpt" {
            ttfpt = true;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--chain=") {
            chain = value.to_owned();
            continue;
        }

        if arg == "--chain" {
            let Some(value) = args.next() else {
                eprintln!("missing value for --chain");
                print_usage_and_exit();
            };
            chain = value;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--bootnode=") {
            bootnodes.push(value.to_owned());
            continue;
        }

        if arg == "--bootnode" {
            let Some(value) = args.next() else {
                eprintln!("missing value for --bootnode");
                print_usage_and_exit();
            };
            bootnodes.push(value);
            continue;
        }

        if arg == "--p2p-log-path" {
            let Some(value) = args.next() else {
                eprintln!("missing value for --p2p-log-path");
                print_usage_and_exit();
            };
            p2p_log_path = Some(PathBuf::from(value));
            continue;
        }

        if arg == "--execution-port" {
            let Some(value) = args.next() else {
                eprintln!("missing value for --execution-port");
                print_usage_and_exit();
            };
            execution_port = Some(value.parse().expect("invalid port"));
            continue;
        }

        if arg == "--consensus-port" {
            let Some(value) = args.next() else {
                eprintln!("missing value for --consensus-port");
                print_usage_and_exit();
            };
            consensus_port = Some(value.parse().expect("invalid port"));
            continue;
        }

        if arg == "--metrics-port" {
            let Some(value) = args.next() else {
                eprintln!("missing value for --metrics-port");
                print_usage_and_exit();
            };
            metrics_port = Some(value.parse().expect("invalid port"));
            continue;
        }

        if arg == "--disable-discovery-v4" {
            discovery_v4 = false;
            continue;
        }

        eprintln!("unrecognized argument: {arg}");
        print_usage_and_exit();
    }

    let source = match (simulate, p2p, ws_url) {
        (true, false, Some(_)) => {
            eprintln!("choose either --simulate or --ws-url, not both");
            print_usage_and_exit();
        }
        (true, true, _) | (false, true, Some(_)) => {
            eprintln!("choose exactly one data source: --simulate, --ws-url, or --p2p");
            print_usage_and_exit();
        }
        (true, false, None) => DataSource::Simulated,
        (false, true, None) => DataSource::P2p {
            chain,
            bootnodes,
            discovery_v4,
            log_path: p2p_log_path,
            execution_port,
            consensus_port,
        },
        (false, false, Some(ws_url)) => DataSource::Live { ws_url },
        (false, false, None) => {
            eprintln!("missing data source: provide --ws-url <url>, use --p2p, or use --simulate");
            print_usage_and_exit();
        }
    };

    CliArgs {
        duration_secs,
        source,
        metrics_port,
        ttfpt,
    }
}

fn parse_duration_value(value: &str) -> u64 {
    match value.parse::<u64>() {
        Ok(duration_secs) => duration_secs,
        Err(_) => {
            eprintln!("invalid duration value: {value}");
            print_usage_and_exit();
        }
    }
}

fn print_usage_and_exit() -> ! {
    eprintln!(
        "usage: cargo run -p mempooloracle --example cli_tool -- ((--ws-url <url>) | (--p2p [--chain <name>] [--bootnode <enode>]...) | --simulate) [--duration <seconds>]"
    );
    eprintln!("default: --duration 30");
    eprintln!("use --duration 0 to run until interrupted");
    eprintln!("or set MEMPOOLORACLE_WS_URL / ETH_WS_URL instead of passing --ws-url");
    process::exit(2);
}

fn simulate_blockchain(
    events: mpsc::Sender<MempoolEvent>,
    stop: Arc<AtomicBool>,
    telemetry: Arc<Mutex<Telemetry>>,
) {
    let mut nonce = 0;
    let sender = Address([1; 20]);
    let mut block_number = 0;

    while !stop.load(Ordering::Relaxed) {
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) {
                return;
            }

            let tx = PendingTx {
                id: ExecutionHash([nonce as u8; 32]),
                sender,
                nonce,
                max_fee_per_gas: 15 + (nonce % 10) as u128,
                max_priority_fee_per_gas: 1 + (nonce % 5) as u128,
                gas_limit: 21000,
                seen_at: std::time::SystemTime::now(),
            };
            if events.send(MempoolEvent::PendingTransaction(tx)).is_err() {
                return;
            }
            telemetry
                .lock()
                .expect("telemetry lock poisoned")
                .pending_seen += 1;
            nonce += 1;
        }

        if should_stop_for(&stop, Duration::from_secs(5)) {
            return;
        }

        let block_tx_count = 5;
        let included_txs: Vec<PendingTx> = (0..block_tx_count)
            .map(|i| {
                let tx_nonce = nonce - 10 + i;
                PendingTx {
                    id: ExecutionHash([tx_nonce as u8; 32]),
                    sender,
                    nonce: tx_nonce,
                    max_fee_per_gas: 15 + (tx_nonce % 10) as u128,
                    max_priority_fee_per_gas: 1 + (tx_nonce % 5) as u128,
                    gas_limit: 21000,
                    seen_at: std::time::SystemTime::now(),
                }
            })
            .collect();

        {
            let mut telemetry = telemetry.lock().expect("telemetry lock poisoned");
            telemetry.block_count = block_number + 1;
            telemetry.last_block_txs = included_txs.len();
            telemetry.last_block_gas_used = 21000 * block_tx_count;
        }

        let block = BlockUpdate {
            number: BlockNumber(block_number),
            hash: mempooloracle::ExecutionHash::from([block_number as u8; 32]),
            parent_hash: mempooloracle::ExecutionHash::from(
                [block_number.saturating_sub(1) as u8; 32],
            ),
            included_txs,
            new_base_fee: 10 + (block_number % 5) as u128,
            gas_used: 21000 * block_tx_count,
            gas_limit: 30_000_000,
        };

        if events.send(MempoolEvent::NewBlock(block)).is_err() {
            return;
        }
        block_number += 1;
    }
}

fn should_stop_for(stop: &AtomicBool, duration: Duration) -> bool {
    let started = Instant::now();

    while started.elapsed() < duration {
        if stop.load(Ordering::Relaxed) {
            return true;
        }

        let remaining = duration.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }

    stop.load(Ordering::Relaxed)
}
