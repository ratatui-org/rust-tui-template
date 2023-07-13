use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::{
  sync::{mpsc, oneshot, Mutex},
  task::JoinHandle,
};
use tracing::debug;

use crate::{
  components::{home::Home, Component},
  event::EventHandler,
  terminal::TerminalHandler,
  trace_dbg,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
  Quit,
  Resume,
  Suspend,
  Tick,
  RenderTick,
  Resize(u16, u16),
  ToggleShowLogger,
  ScheduleIncrementCounter,
  ScheduleDecrementCounter,
  AddToCounter(usize),
  SubtractFromCounter(usize),
  EnterNormal,
  EnterInsert,
  EnterProcessing,
  ExitProcessing,
  Update,
  Noop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
  Render,
  Stop,
}

pub struct App {
  pub tick_rate: (u64, u64),
  pub home: Arc<Mutex<Home>>,
}

impl App {
  pub fn new(tick_rate: (u64, u64)) -> Result<Self> {
    let home = Arc::new(Mutex::new(Home::new()));
    Ok(Self { tick_rate, home })
  }

  pub fn spawn_tui_task(&mut self) -> (JoinHandle<()>, mpsc::UnboundedSender<Message>) {
    let home = self.home.clone();

    let (tui_tx, mut tui_rx) = mpsc::unbounded_channel::<Message>();

    let tui_task = tokio::spawn(async move {
      let mut tui = TerminalHandler::new().context(anyhow!("Unable to create TUI")).unwrap();
      tui.enter().unwrap();
      loop {
        match tui_rx.recv().await {
          Some(Message::Stop) => break,
          Some(Message::Render) => {
            let mut h = home.lock().await;
            tui
              .terminal
              .draw(|f| {
                h.render(f, f.size());
              })
              .unwrap();
          },
          None => {},
        }
      }
      tui.exit().unwrap();
    });

    (tui_task, tui_tx)
  }

  pub fn spawn_event_task(&mut self, tx: mpsc::UnboundedSender<Action>) -> (JoinHandle<()>, oneshot::Sender<()>) {
    let home = self.home.clone();
    let (app_tick_rate, render_tick_rate) = self.tick_rate;
    let (stop_event_tx, mut stop_event_rx) = oneshot::channel::<()>();
    let event_task = tokio::spawn(async move {
      let mut events = EventHandler::new(app_tick_rate, render_tick_rate);
      loop {
        let event = events.next().await;
        let action = home.lock().await.handle_events(event);
        tx.send(action).unwrap();
        if stop_event_rx.try_recv().ok().is_some() {
          events.stop().await.unwrap();
          break;
        }
      }
    });
    (event_task, stop_event_tx)
  }

  pub async fn run(&mut self) -> Result<()> {
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();

    self.home.lock().await.action_tx = Some(action_tx.clone());

    self.home.lock().await.init()?;

    let (mut tui_task, mut tui_tx) = self.spawn_tui_task();
    let (mut event_task, mut stop_event_tx) = self.spawn_event_task(action_tx.clone());

    loop {
      let mut maybe_action = action_rx.recv().await;
      while maybe_action.is_some() {
        let action = maybe_action.unwrap();
        if action == Action::RenderTick {
          tui_tx.send(Message::Render).unwrap_or(());
        } else if action != Action::Tick {
          trace_dbg!(action.clone());
        }
        if let Some(a) = self.home.lock().await.dispatch(action) {
          action_tx.send(a)?
        };
        maybe_action = action_rx.try_recv().ok();
      }

      if self.home.lock().await.should_suspend {
        tui_tx.send(Message::Stop).unwrap_or(());
        stop_event_tx.send(()).unwrap_or(());
        tui_task.await?;
        event_task.await?;
        let tui = TerminalHandler::new().context(anyhow!("Unable to create TUI")).unwrap();
        tui.suspend()?; // Blocks here till process resumes on Linux and Mac.
                        // TODO: figure out appropriate behaviour on Windows.
        debug!("resuming");
        (tui_task, tui_tx) = self.spawn_tui_task();
        (event_task, stop_event_tx) = self.spawn_event_task(action_tx.clone());
        action_tx.send(Action::Resume)?;
      } else if self.home.lock().await.should_quit {
        tui_tx.send(Message::Stop).unwrap_or(());
        stop_event_tx.send(()).unwrap_or(());
        tui_task.await?;
        event_task.await?;
        break;
      }
    }
    Ok(())
  }
}
