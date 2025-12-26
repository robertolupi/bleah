use std::io;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use btleplug::api::{Central as _, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager;
use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use tokio::sync::watch;

#[derive(Clone, Debug)]
struct DeviceInfo {
    id: String,
    name: String,
    rssi: Option<i16>,
    connected: bool,
}

#[derive(Debug)]
enum Message {
    Devices(Vec<DeviceInfo>),
    Status(String),
}

struct AppState {
    devices: Vec<DeviceInfo>,
    status: String,
}

impl AppState {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            status: "Starting scan...".to_string(),
        }
    }

    fn apply(&mut self, msg: Message) {
        match msg {
            Message::Devices(mut devices) => {
                devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
                self.devices = devices;
                self.status = "Scanning...".to_string();
            }
            Message::Status(status) => self.status = status,
        }
    }
}

fn main() -> Result<()> {
    let mut stdout = io::stdout();
    crossterm::terminal::enable_raw_mode().context("enable raw mode")?;
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = run_app(&mut terminal);

    crossterm::terminal::disable_raw_mode().context("disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")?;

    result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Message>();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .context("build tokio runtime")?;
    runtime.spawn(scan_loop(tx, shutdown_rx));

    let mut state = AppState::new();
    let tick_rate = Duration::from_millis(250);

    loop {
        while let Ok(msg) = rx.try_recv() {
            state.apply(msg);
        }

        terminal.draw(|frame| draw_ui(frame, &state))?;

        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                    break;
                }
            }
        }
    }

    let _ = shutdown_tx.send(true);
    runtime.shutdown_timeout(Duration::from_secs(1));

    Ok(())
}

fn draw_ui(frame: &mut Frame, state: &AppState) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(frame.size());

    let title = Line::from(vec![
        Span::styled("bleah", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" - BLE devices "),
        Span::styled(
            format!("({})", state.devices.len()),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::raw(" "),
        Span::styled(
            state.status.clone(),
            Style::default().fg(Color::Yellow),
        ),
    ]);
    frame.render_widget(Paragraph::new(title), layout[0]);

    let header = Row::new(vec![
        Cell::from("Address"),
        Cell::from("Name"),
        Cell::from("RSSI"),
        Cell::from("Connected"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows = state.devices.iter().map(|device| {
        let rssi = device
            .rssi
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let connected = if device.connected { "yes" } else { "no" };
        Row::new(vec![
            Cell::from(device.id.clone()),
            Cell::from(device.name.clone()),
            Cell::from(rssi),
            Cell::from(connected),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Min(10),
            Constraint::Length(6),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .block(Block::default().title("Nearby devices").borders(Borders::ALL))
    .column_spacing(1);

    frame.render_widget(table, layout[1]);

    let help = Paragraph::new("q/esc to quit");
    frame.render_widget(help, layout[2]);
}

async fn scan_loop(tx: mpsc::Sender<Message>, mut shutdown: watch::Receiver<bool>) {
    let manager = match Manager::new().await {
        Ok(manager) => manager,
        Err(err) => {
            let _ = tx.send(Message::Status(format!("BLE manager error: {err}")));
            return;
        }
    };

    let adapters = match manager.adapters().await {
        Ok(adapters) => adapters,
        Err(err) => {
            let _ = tx.send(Message::Status(format!("Adapter discovery error: {err}")));
            return;
        }
    };

    let Some(adapter) = adapters.into_iter().next() else {
        let _ = tx.send(Message::Status("No BLE adapters found".to_string()));
        return;
    };

    if let Err(err) = adapter.start_scan(ScanFilter::default()).await {
        let _ = tx.send(Message::Status(format!("Scan failed: {err}")));
        return;
    }

    let mut interval = tokio::time::interval(Duration::from_secs(2));

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            _ = interval.tick() => {
                let peripherals = match adapter.peripherals().await {
                    Ok(peripherals) => peripherals,
                    Err(err) => {
                        let _ = tx.send(Message::Status(format!("Scan error: {err}")));
                        continue;
                    }
                };

                let mut devices = Vec::new();
                for peripheral in peripherals {
                    let id = peripheral.id().to_string();
                    let props = peripheral.properties().await.ok().flatten();
                    let name = props
                        .as_ref()
                        .and_then(|props| props.local_name.clone())
                        .unwrap_or_else(|| "Unknown".to_string());
                    let rssi = props.as_ref().and_then(|props| props.rssi);
                    let connected = peripheral.is_connected().await.unwrap_or(false);

                    devices.push(DeviceInfo {
                        id,
                        name,
                        rssi,
                        connected,
                    });
                }

                let _ = tx.send(Message::Devices(devices));
            }
        }
    }
}
