use iocraft::prelude::*;
use std::{
    sync::{mpsc, Arc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Clone, Copy)]
pub(super) enum StatusColor {
    Cyan,
    Green,
    Red,
}

#[derive(Clone)]
struct StatusLine {
    color: Color,
    content: String,
}

#[derive(Clone, Default)]
struct TuiState {
    status: Vec<StatusLine>,
    logs: Vec<String>,
    finished: bool,
}

impl TuiState {
    fn push_status(&mut self, line: StatusLine) {
        self.status.push(line);
        if self.status.len() > 2_000 {
            self.status.drain(..self.status.len() - 2_000);
        }
    }

    fn push_logs(&mut self, lines: Vec<String>) {
        self.logs.extend(lines);
        if self.logs.len() > 20_000 {
            self.logs.drain(..self.logs.len() - 20_000);
        }
    }
}

enum TuiMessage {
    Status(StatusColor, String),
    Logs(Vec<String>),
    Finished,
}

#[derive(Default, Props)]
struct DeploymentViewProps {
    receiver: Option<Arc<Mutex<mpsc::Receiver<TuiMessage>>>>,
}

#[component]
fn DeploymentView(
    props: &mut DeploymentViewProps,
    mut hooks: Hooks,
) -> impl Into<AnyElement<'static>> {
    let mut state = hooks.use_state(TuiState::default);
    let mut system = hooks.use_context_mut::<SystemContext>();
    let receiver = props
        .receiver
        .clone()
        .expect("deployment view requires an event receiver");

    hooks.use_future(async move {
        loop {
            let messages = {
                let receiver = receiver.lock().unwrap();
                let mut messages = Vec::new();
                loop {
                    match receiver.try_recv() {
                        Ok(message) => messages.push(message),
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            messages.push(TuiMessage::Finished);
                            break;
                        }
                    }
                }
                messages
            };
            if messages.is_empty() {
                smol::Timer::after(Duration::from_millis(25)).await;
                continue;
            }

            let mut current = state.read().clone();
            for message in messages {
                match message {
                    TuiMessage::Status(color, content) => {
                        let color = match color {
                            StatusColor::Cyan => Color::Cyan,
                            StatusColor::Green => Color::Green,
                            StatusColor::Red => Color::Red,
                        };
                        current.push_status(StatusLine { color, content });
                    }
                    TuiMessage::Logs(lines) => current.push_logs(lines),
                    TuiMessage::Finished => current.finished = true,
                }
            }
            state.set(current);
            if state.read().finished {
                break;
            }
        }
    });

    let (_, terminal_height) = hooks.use_terminal_size();
    let state = state.read().clone();
    if state.finished {
        system.exit();
    }
    let status_rows = (usize::from(terminal_height) * 2 / 3).saturating_sub(2);
    let status_start = state.status.len().saturating_sub(status_rows);
    let status_lines = state.status[status_start..].iter().map(|line| {
        element! {
            Text(color: line.color, weight: Weight::Bold, content: line.content.clone())
        }
    });
    let log_lines = state.logs.iter().map(|line| {
        element! {
            Text(color: Color::Grey, wrap: TextWrap::NoWrap, content: line.clone())
        }
    });

    element! {
        View(width: 100pct, height: 100pct, flex_direction: FlexDirection::Column) {
            View(width: 100pct, height: 1, padding_left: 1) {
                Text(color: Color::Cyan, weight: Weight::Bold, content: "Cattix deployment")
            }
            View(width: 100pct, flex_grow: 1.0_f32, flex_direction: FlexDirection::Column, overflow: Overflow::Hidden, padding_left: 1) {
                #(status_lines)
            }
            View(width: 100pct, height: Percent(33.0), flex_direction: FlexDirection::Column, border_style: BorderStyle::Round, border_color: Some(Color::DarkGrey)) {
                View(width: 100pct, height: 1, padding_left: 1) {
                    Text(color: Color::White, weight: Weight::Bold, content: "Build and deployment logs")
                }
                View(width: 100pct, flex_grow: 1.0_f32) {
                    ScrollView(auto_scroll: true, scrollbar: Some(true)) {
                        View(width: 100pct, flex_direction: FlexDirection::Column, padding_left: 1) {
                            #(log_lines)
                        }
                    }
                }
            }
        }
    }
}

pub(super) struct DeploymentTui {
    sender: mpsc::Sender<TuiMessage>,
    thread: Option<JoinHandle<()>>,
    last_status: Option<String>,
}

impl DeploymentTui {
    pub(super) fn start() -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let receiver = Arc::new(Mutex::new(receiver));
        let thread = thread::Builder::new()
            .name("cattix-deployment-tui".into())
            .spawn(move || {
                let mut view = element!(DeploymentView(receiver: receiver));
                if let Err(error) = smol::block_on(view.render_loop()) {
                    eprintln!("Deployment TUI stopped: {error}");
                }
            })?;

        Ok(Self {
            sender,
            thread: Some(thread),
            last_status: None,
        })
    }

    pub(super) fn status(&mut self, color: StatusColor, message: String) -> std::io::Result<()> {
        self.last_status = Some(message.clone());
        self.sender
            .send(TuiMessage::Status(color, message))
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::BrokenPipe, error))
    }

    pub(super) fn logs(&self, lines: Vec<String>) -> std::io::Result<()> {
        self.sender
            .send(TuiMessage::Logs(lines))
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::BrokenPipe, error))
    }
}

impl Drop for DeploymentTui {
    fn drop(&mut self) {
        let _ = self.sender.send(TuiMessage::Finished);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(message) = &self.last_status {
            eprintln!("{message}");
        }
    }
}
