use std::io;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use bleah::{DetailItem, DeviceInfo, PeripheralDecoder, ScanMessage};
use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use tokio::sync::watch;

struct AppState {
    devices: Vec<DeviceInfo>,
    status: String,
    selected_id: Option<String>,
    table_state: TableState,
}

impl AppState {
    fn new() -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self {
            devices: Vec::new(),
            status: "Starting scan...".to_string(),
            selected_id: None,
            table_state,
        }
    }

    fn apply(&mut self, msg: ScanMessage) {
        match msg {
            ScanMessage::Devices(mut devices) => {
                devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
                let selected_id = self
                    .selected_id
                    .clone()
                    .or_else(|| self.selected_device().map(|device| device.id.clone()));
                self.devices = devices;
                let selected_index = selected_id
                    .as_ref()
                    .and_then(|id| self.devices.iter().position(|device| device.id == *id))
                    .or_else(|| if self.devices.is_empty() { None } else { Some(0) });
                self.table_state.select(selected_index);
                self.selected_id = selected_index
                    .and_then(|index| self.devices.get(index))
                    .map(|device| device.id.clone());
                self.status = "Scanning...".to_string();
            }
            ScanMessage::Status(status) => self.status = status,
        }
    }

    fn selected_device(&self) -> Option<&DeviceInfo> {
        self.table_state
            .selected()
            .and_then(|index| self.devices.get(index))
    }

    fn select_next(&mut self) {
        if self.devices.is_empty() {
            self.table_state.select(None);
            self.selected_id = None;
            return;
        }
        let next = match self.table_state.selected() {
            Some(index) => (index + 1) % self.devices.len(),
            None => 0,
        };
        self.table_state.select(Some(next));
        self.selected_id = self.devices.get(next).map(|device| device.id.clone());
    }

    fn select_previous(&mut self) {
        if self.devices.is_empty() {
            self.table_state.select(None);
            self.selected_id = None;
            return;
        }
        let next = match self.table_state.selected() {
            Some(index) if index > 0 => index - 1,
            Some(_) | None => self.devices.len() - 1,
        };
        self.table_state.select(Some(next));
        self.selected_id = self.devices.get(next).map(|device| device.id.clone());
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
    let (tx, rx) = mpsc::channel::<ScanMessage>();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .context("build tokio runtime")?;
    runtime.spawn(bleah::scan_loop(tx, shutdown_rx));

    let mut state = AppState::new();
    let decoders = bleah::default_decoders();
    let tick_rate = Duration::from_millis(250);

    loop {
        while let Ok(msg) = rx.try_recv() {
            state.apply(msg);
        }

        terminal.draw(|frame| draw_ui(frame, &mut state, &decoders))?;

        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down => state.select_next(),
                    KeyCode::Up => state.select_previous(),
                    _ => {}
                }
            }
        }
    }

    let _ = shutdown_tx.send(true);
    runtime.shutdown_timeout(Duration::from_secs(1));

    Ok(())
}

fn draw_ui(frame: &mut Frame, state: &mut AppState, decoders: &[Box<dyn PeripheralDecoder>]) {
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

    let content = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(layout[1]);

    let header = Row::new(vec![
        Cell::from("Address"),
        Cell::from("Name"),
        Cell::from("RSSI"),
        Cell::from("Connected"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows = state.devices.iter().map(|device| {
        let summary = device_summary(device, decoders);
        let mut name_spans = vec![Span::styled(
            device.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if let Some(extra) = summary {
            name_spans.push(Span::raw(" "));
            name_spans.push(Span::raw(extra));
        }
        let rssi = device
            .rssi
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let connected = if device.connected { "yes" } else { "no" };
        Row::new(vec![
            Cell::from(device.id.clone()),
            Cell::from(Line::from(name_spans)),
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
    .column_spacing(1)
    .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    frame.render_stateful_widget(table, content[0], &mut state.table_state);

    let details = details_panel(state.selected_device(), decoders);
    frame.render_widget(details, content[1]);

    let help = Paragraph::new("up/down to select, q/esc to quit");
    frame.render_widget(help, layout[2]);
}

fn device_summary(
    device: &DeviceInfo,
    decoders: &[Box<dyn PeripheralDecoder>],
) -> Option<String> {
    decoders.iter().find_map(|decoder| decoder.summary(device))
}

fn details_panel(
    device: Option<&DeviceInfo>,
    decoders: &[Box<dyn PeripheralDecoder>],
) -> Paragraph<'static> {
    let lines = match device {
        Some(device) => device_details(device, decoders),
        None => vec![Line::from("No device selected.")],
    };

    Paragraph::new(lines)
        .block(Block::default().title("Device details").borders(Borders::ALL))
        .wrap(Wrap { trim: false })
}

fn device_details(
    device: &DeviceInfo,
    decoders: &[Box<dyn PeripheralDecoder>],
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    lines.push(Line::from(vec![Span::styled(
        device.name.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(format!("Address: {}", device.id)));
    lines.push(Line::from(format!(
        "Connected: {}",
        if device.connected { "yes" } else { "no" }
    )));
    lines.push(Line::from(format!(
        "RSSI: {}",
        device
            .rssi
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    )));
    if let Some(tx_power) = device.tx_power_level {
        lines.push(Line::from(format!("Tx power: {tx_power}")));
    }
    if let Some(address_type) = device.address_type {
        lines.push(Line::from(format!("Address type: {address_type:?}")));
    }
    if device.services.is_empty() {
        lines.push(Line::from("Services: -"));
    } else {
        lines.push(Line::from(format!("Services: {}", device.services.join(", "))));
    }

    let decoded = decoded_details(device, decoders);
    if !decoded.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "Decoded data",
            Style::default().add_modifier(Modifier::BOLD),
        )]));
        for item in decoded {
            lines.push(Line::from(format!("{}: {}", item.label, item.value)));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "Manufacturer data",
        Style::default().add_modifier(Modifier::BOLD),
    )]));
    if device.manufacturer_data.is_empty() {
        lines.push(Line::from("-"));
    } else {
        for (company_id, data) in &device.manufacturer_data {
            lines.push(Line::from(format!(
                "0x{company_id:04x}: {}",
                bleah::hex_bytes(data)
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "Service data",
        Style::default().add_modifier(Modifier::BOLD),
    )]));
    if device.service_data.is_empty() {
        lines.push(Line::from("-"));
    } else {
        for (uuid, data) in &device.service_data {
            lines.push(Line::from(format!("{uuid}: {}", bleah::hex_bytes(data))));
        }
    }

    lines
}

fn decoded_details(
    device: &DeviceInfo,
    decoders: &[Box<dyn PeripheralDecoder>],
) -> Vec<DetailItem> {
    decoders
        .iter()
        .flat_map(|decoder| decoder.details(device))
        .collect()
}
