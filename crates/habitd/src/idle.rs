//! Idle detection through the `ext-idle-notify-v1` Wayland protocol.
//!
//! The compositor watches for input and reports once there was none for the
//! timeout habitd asked for (`idle_timeout`).

use crate::Event;
use habit_core::Input;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

struct IdleState {
    tx: Sender<Event>,
    /// Generation of the watcher thread that is current.
    current: Arc<AtomicU64>,
    /// Generation of this thread.
    generation: u64,
}

impl IdleState {
    fn stale(&self) -> bool {
        self.current.load(Ordering::Relaxed) != self.generation
    }
}

/// Keeps one idle notification with the configured timeout. A timeout only
/// applies to notifications created with it, so a new one replaces the old
/// whenever the setting changes.
#[derive(Default)]
pub struct Watch {
    timeout_ms: Option<u64>,
    current: Arc<AtomicU64>,
}

impl Watch {
    /// Starts watching, or watches again when `timeout_ms` changed since the
    /// last call.
    pub fn poll(&mut self, timeout_ms: u64, tx: &Sender<Event>) {
        if self.timeout_ms == Some(timeout_ms) {
            return;
        }
        if self.timeout_ms.is_some() {
            // The new notification starts counting now and only reports a
            // return after it reported idle, so start out active.
            let _ = tx.try_send(Event::Input(Input::Idle(false)));
        }
        self.timeout_ms = Some(timeout_ms);
        // The old thread stops at its next event, which it ignores.
        let generation = self.current.fetch_add(1, Ordering::Relaxed) + 1;
        let state = IdleState { tx: tx.clone(), current: self.current.clone(), generation };
        std::thread::spawn(move || {
            if let Err(e) = run(state, timeout_ms) {
                eprintln!("habitd: idle detection disabled: {e}");
            }
        });
    }
}

fn run(mut state: IdleState, timeout_ms: u64) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<IdleState>(&conn)?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ())?;
    let notifier: ExtIdleNotifierV1 = globals.bind(&qh, 1..=2, ())?;
    let timeout = u32::try_from(timeout_ms).unwrap_or(u32::MAX);
    // v2 ignores idle inhibitors: a video playing in the background must not
    // count as reading.
    let _notification = if notifier.version() >= 2 {
        notifier.get_input_idle_notification(timeout, &seat, &qh, ())
    } else {
        notifier.get_idle_notification(timeout, &seat, &qh, ())
    };
    loop {
        queue.blocking_dispatch(&mut state)?;
        if state.tx.is_closed() || state.stale() {
            return Ok(());
        }
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for IdleState {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let idle = match event {
            ext_idle_notification_v1::Event::Idled => true,
            ext_idle_notification_v1::Event::Resumed => false,
            _ => return,
        };
        if state.stale() {
            return;
        }
        let _ = state.tx.blocking_send(Event::Input(Input::Idle(idle)));
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for IdleState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for IdleState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for IdleState {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
