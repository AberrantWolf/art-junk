//! Actor-thread engine, Command/Event plumbing, and snapshot publication for art-junk.

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use aj_core::{Point, SceneSnapshot, Stroke, StrokeId};
use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, unbounded};

#[derive(Debug, Clone, Copy)]
pub enum Command {
    BeginStroke { id: StrokeId, point: Point },
    AddSample { id: StrokeId, point: Point },
    EndStroke { id: StrokeId },
    Shutdown,
}

pub struct Engine {
    tx: Sender<Command>,
    snapshot: Arc<ArcSwap<SceneSnapshot>>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    #[must_use]
    pub fn spawn() -> Self {
        let (tx, rx) = unbounded();
        let snapshot = Arc::new(ArcSwap::new(Arc::new(SceneSnapshot::default())));
        let snap_for_thread = snapshot.clone();
        let thread = thread::Builder::new()
            .name("aj-engine".into())
            .spawn(move || run_actor(&rx, &snap_for_thread))
            .expect("spawn aj-engine thread");
        Self { tx, snapshot, thread: Some(thread) }
    }

    pub fn send(&self, cmd: Command) {
        if let Err(err) = self.tx.send(cmd) {
            log::warn!("aj-engine send on closed channel: {err:?}");
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<SceneSnapshot> {
        self.snapshot.load_full()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn run_actor(rx: &Receiver<Command>, snapshot: &Arc<ArcSwap<SceneSnapshot>>) {
    let mut strokes: Vec<Stroke> = Vec::new();
    let mut live: Option<usize> = None;
    while let Ok(first) = rx.recv() {
        let mut stop = apply(&mut strokes, &mut live, first);
        while let Ok(cmd) = rx.try_recv() {
            if apply(&mut strokes, &mut live, cmd) {
                stop = true;
            }
        }
        snapshot.store(Arc::new(SceneSnapshot { strokes: strokes.clone() }));
        if stop {
            break;
        }
    }
}

fn apply(strokes: &mut Vec<Stroke>, live: &mut Option<usize>, cmd: Command) -> bool {
    match cmd {
        Command::BeginStroke { id, point } => {
            strokes.push(Stroke { id, points: vec![point] });
            *live = Some(strokes.len() - 1);
        }
        Command::AddSample { id, point } => {
            if let Some(idx) = *live
                && strokes.get(idx).is_some_and(|s| s.id == id)
            {
                strokes[idx].points.push(point);
            }
        }
        Command::EndStroke { id } => {
            if let Some(idx) = *live
                && strokes.get(idx).is_some_and(|s| s.id == id)
            {
                *live = None;
            }
        }
        Command::Shutdown => return true,
    }
    false
}
