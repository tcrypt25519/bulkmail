use alloy::providers::{ProviderBuilder, WsConnect};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use mempooloracle::{
    Address, AlloyTrackerRuntime, BlockUpdate, MempoolEvent, MempoolHandle, MempoolTracker,
    PendingTx, TrackerConfig, TxId,
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
    io::IsTerminal,
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
    let args = parse_args();
    let stop = Arc::new(AtomicBool::new(false));
    let telemetry = Arc::new(Mutex::new(Telemetry::default()));
    let config = TrackerConfig {
        initial_base_fee: 10,
        global_capacity: 1000,
        per_account_capacity: 100,
        private_flow_prior: 0.1,
    };

    let runtime = match args.source {
        DataSource::Live { ws_url } => Runtime::Live(
            MempoolTracker::connect_with_builder(
                ProviderBuilder::default(),
                WsConnect::new(ws_url),
                config,
            )
            .await
            .map_err(|err| io::Error::other(format!("failed to connect mempool oracle: {err}")))?,
        ),
        DataSource::Simulated => {
            let (event_sender, event_receiver) = mpsc::channel();
            let (handle, tracker) = MempoolTracker::new(event_receiver, &[], config);
            let tracker_thread = thread::spawn(move || tracker.run());

            let simulation_stop = stop.clone();
            let telemetry_clone = telemetry.clone();
            let simulation_thread = thread::spawn(move || {
                simulate_blockchain(event_sender, simulation_stop, telemetry_clone)
            });

            Runtime::Simulated {
                handle,
                simulation_thread,
                tracker_thread,
            }
        }
    };

    let handle = runtime.handle();

    let result = if io::stdout().is_terminal() {
        let terminal = ratatui::init();
        let result = run_app(
            terminal,
            handle,
            args.duration_secs,
            stop.clone(),
            telemetry,
        );
        ratatui::restore();
        result
    } else {
        run_plaintext(handle, args.duration_secs, stop.clone(), telemetry)
    };

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
}

enum DataSource {
    Live { ws_url: String },
    Simulated,
}

fn run_app(
    mut terminal: DefaultTerminal,
    handle: MempoolHandle,
    duration_secs: u64,
    stop: Arc<AtomicBool>,
    telemetry: Arc<Mutex<Telemetry>>,
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
    telemetry: Arc<Mutex<Telemetry>>,
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
            println!(
                "base_fee={} min_tip={} expected_txs={} private_flow={:.1}% blocks={} pending_seen={}",
                snapshot.base_fee,
                snapshot.min_tip,
                snapshot.expected_txs,
                snapshot.private_flow,
                snapshot.block_count,
                snapshot.pending_seen
            );
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
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
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

    render_metrics(frame, columns[0], app);
    render_base_fee_chart(frame, columns[1], app);
    render_top_transactions(frame, columns[2], app);
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
    pending_seen: u64,
    last_block_txs: usize,
    last_block_gas_used: u64,
    ranked_rows: Vec<TxRow>,
}

impl Snapshot {
    fn capture(
        handle: &MempoolHandle,
        telemetry: &Arc<Mutex<Telemetry>>,
        started_at: Instant,
    ) -> Self {
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

        let telemetry = telemetry.lock().expect("telemetry lock poisoned").clone();

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
            pending_seen: telemetry.pending_seen,
            last_block_txs: telemetry.last_block_txs,
            last_block_gas_used: telemetry.last_block_gas_used,
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
    last_block_txs: usize,
    last_block_gas_used: u64,
}

fn parse_args() -> CliArgs {
    let mut args = env::args().skip(1);
    let mut duration_secs = 30_u64;
    let mut ws_url = env::var("MEMPOOLORACLE_WS_URL")
        .ok()
        .or_else(|| env::var("ETH_WS_URL").ok());
    let mut simulate = false;

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

        eprintln!("unrecognized argument: {arg}");
        print_usage_and_exit();
    }

    let source = match (simulate, ws_url) {
        (true, Some(_)) => {
            eprintln!("choose either --simulate or --ws-url, not both");
            print_usage_and_exit();
        }
        (true, None) => DataSource::Simulated,
        (false, Some(ws_url)) => DataSource::Live { ws_url },
        (false, None) => {
            eprintln!("missing data source: provide --ws-url <url> or use --simulate");
            print_usage_and_exit();
        }
    };

    CliArgs {
        duration_secs,
        source,
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
        "usage: cargo run -p mempooloracle --example cli_tool -- (--ws-url <url> | --simulate) [--duration <seconds>]"
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
                id: TxId([nonce as u8; 32]),
                sender,
                nonce,
                max_fee_per_gas: 15 + (nonce % 10) as u128,
                max_priority_fee_per_gas: 1 + (nonce % 5) as u128,
                gas_limit: 21000,
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
                    id: TxId([tx_nonce as u8; 32]),
                    sender,
                    nonce: tx_nonce,
                    max_fee_per_gas: 15 + (tx_nonce % 10) as u128,
                    max_priority_fee_per_gas: 1 + (tx_nonce % 5) as u128,
                    gas_limit: 21000,
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
