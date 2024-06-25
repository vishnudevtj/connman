use std::{
    io::{self, stdout, Stdout},
    sync::mpsc::{self, Receiver, Sender},
    thread::spawn,
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::*,
};
use log::{error};
use ratatui::{
    prelude::*,
    widgets::{
        block::{Position, Title},
        Block, Borders, Paragraph, Row, Table, TableState,
    },
};
use symbols::border;
use tokio::sync::OnceCell;

use crate::{docker::CONTAINER_NAME_PREFIX};

pub static TUI_SENDER: OnceCell<Sender<Msg>> = OnceCell::const_new();

pub type TuiSender = Sender<Msg>;

/// A type alias for the terminal type used in this application
pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub enum Msg {
    Proxy(ProxyInfo),
    // Here the u64 represent the ProxyId,
    Remove(u64),
    Log(LogInfo),
}

#[derive(Debug)]
pub struct ProxyInfo {
    pub id: u64,
    pub host_port: u16,
    pub docker_image: String,
    pub docker_host: String,
    pub docker_port: u16,
}

#[derive(Debug)]
pub struct LogInfo {
    pub id: u64,
    pub log: String,
}

/// Initialize the terminal
pub fn init() -> io::Result<Tui> {
    execute!(stdout(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    Terminal::new(CrosstermBackend::new(stdout()))
}

/// Restore the terminal to its original state
pub fn restore() -> io::Result<()> {
    execute!(stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

pub struct App {
    proxy_table_state: TableState,
    selected_proxy: u64,
    receiver: Receiver<Msg>,
    sender: Sender<Msg>,
    proxy: ProxyList,
    log: LogList,
    exit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        let proxy_table_state = TableState::default();
        let proxy = Default::default();
        let log = Default::default();
        let exit = false;

        Self {
            proxy_table_state,
            selected_proxy: 0,
            sender,
            receiver,
            proxy,
            log,
            exit,
        }
    }

    pub fn sender(&self) -> Sender<Msg> {
        self.sender.clone()
    }

    fn handle_message(&mut self, msg: Msg) {
        match msg {
            Msg::Proxy(proxy) => {
                self.proxy.0.push(proxy);
            }
            Msg::Log(log) => {
                self.log.0.push(log);
            }
            Msg::Remove(proxy_id) => {
                let id = proxy_id;
                self.proxy.0.retain(|x| x.id != id);
                self.log.0.retain(|x| x.id != id);
            }
        }
    }

    fn init_tui_logger() -> anyhow::Result<()> {
        tui_logger::init_logger(log::LevelFilter::Info)?;
        tui_logger::set_log_file("output.log")?;
        tui_logger::set_default_level(log::LevelFilter::Trace);
        Ok(())
    }

    /// runs the application's main loop until the user quits
    pub fn run(&mut self, terminal: &mut Tui) -> anyhow::Result<()> {
        Self::init_tui_logger()?;
        while !self.exit {
            if let Ok(msg) = self.receiver.try_recv() {
                self.handle_message(msg);
            }
            terminal.draw(|frame| self.render_frame(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn render_frame(&mut self, frame: &mut Frame) {
        let border = Layout::default()
            .constraints(vec![Constraint::Percentage(80), Constraint::Percentage(20)])
            .split(frame.size());

        let tui_logger = tui_logger::TuiLoggerWidget::default().block(
            Block::new()
                .title("System Logs")
                .borders(Borders::ALL)
                .border_set(border::THICK),
        );

        frame.render_widget(&*self, border[0]);
        frame.render_widget(tui_logger, border[1]);

        let layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![Constraint::Percentage(40), Constraint::Percentage(60)])
            .margin(2)
            .split(border[0]);

        let proxy_list = self.proxy.render();
        let log_list = self.log.render(self.selected_proxy);

        frame.render_stateful_widget(proxy_list, layout[0], &mut self.proxy_table_state);

        frame.render_widget(log_list, layout[1]);
    }

    /// updates the application's state based on user input
    fn handle_events(&mut self) -> io::Result<()> {
        loop {
            if crossterm::event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    // it's important to check that the event is a key press event as
                    // crossterm also emits key release and repeat events on Windows.
                    Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                        self.handle_key_event(key_event)
                    }
                    _ => {}
                };
            } else {
                return Ok(());
            }
        }
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Char('q') => self.exit(),
            KeyCode::Up => self.scroll_up(),
            KeyCode::Down => self.scroll_down(),
            _ => {}
        }
    }

    fn exit(&mut self) {
        self.exit = true;
    }

    fn scroll_down(&mut self) {
        let i = match self.proxy_table_state.selected() {
            Some(i) => {
                if i >= self.proxy.0.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.proxy_table_state.select(Some(i));

        // Select that proxy
        if let Some(proxy_info) = self.proxy.0.get(i) {
            self.selected_proxy = proxy_info.id;
        }
    }

    fn scroll_up(&mut self) {
        let i = match self.proxy_table_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.proxy.0.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.proxy_table_state.select(Some(i));

        // Select that proxy
        if let Some(proxy_info) = self.proxy.0.get(i) {
            self.selected_proxy = proxy_info.id;
        }
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title = Title::from(" Connman v0.1 ".bold());
        let instructions = Title::from(Line::from(vec![
            " Select Proxy ".into(),
            " <Up> / <Down> ".blue().bold(),
            " Quit ".into(),
            " <Q> ".blue().bold(),
        ]));

        let block = Block::default()
            .title(title.alignment(Alignment::Center))
            .title(
                instructions
                    .alignment(Alignment::Center)
                    .position(Position::Bottom),
            );

        Paragraph::new("").centered().block(block).render(area, buf);
    }
}

#[derive(Debug, Default)]
struct ProxyList(Vec<ProxyInfo>);

#[derive(Debug, Default)]
struct LogList(Vec<LogInfo>);

impl ProxyList {
    fn render(&self) -> Table {
        let header = vec!["Container Name", "Docker Image", "Mapping"];
        let rows = self.0.iter().map(|proxy| {
            Row::new(vec![
                format!("{}{}", CONTAINER_NAME_PREFIX, proxy.id.to_string()),
                proxy.docker_image.clone(),
                format!(
                    "{} -> {}:{}",
                    proxy.host_port, proxy.docker_host, proxy.docker_port
                ),
            ])
        });
        let widths = [
            Constraint::Percentage(30),
            Constraint::Percentage(40),
            Constraint::Percentage(30),
        ];

        let table = Table::new(rows, widths)
            .column_spacing(1)
            .highlight_style(Style::new().reversed())
            .header(Row::new(header).style(Style::new().bold()).bottom_margin(1))
            .block(
                Block::new()
                    .title("Proxy")
                    .borders(Borders::ALL)
                    .border_set(border::THICK),
            )
            .highlight_symbol(">>");
        table
    }
}

impl LogList {
    fn render(&self, selected_proxy: u64) -> Table {
        let rows = self
            .0
            .iter()
            .filter(|log| log.id == selected_proxy)
            .map(|log| Row::new(vec![log.log.clone()]));
        let widths = [Constraint::Fill(1)];
        let table = Table::new(rows, widths).block(
            Block::new()
                .title("Logs")
                .borders(Borders::ALL)
                .border_set(border::THICK),
        );
        table
    }
}

fn run(mut app: App) -> anyhow::Result<()> {
    let mut terminal = init()?;
    let result = app.run(&mut terminal);
    restore()?;
    result
}

pub fn tui() {
    let app = App::new();
    let sender = app.sender();
    let function = || {
        let _ = run(app).map_err(|err| error!("TUI returned error: {}", err));
    };
    spawn(function);

    TUI_SENDER.set(sender);
}
