//! Explicitly authorized, keyboard-only input through the Wayland desktop portal.
//!
//! Only startup or explicit setup may show consent. Output calls require an approved live
//! session and never fall back to X11, raw devices, or another input backend.
//! The blocking entry points belong on workers, never the GTK/main thread.

use crate::settings::{AutoSubmitKey, PasteMethod};
use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::{PersistMode, Session};
use ashpd::zbus::{fdo::DBusProxy, Connection};
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tokio::task::JoinHandle;

const APP_ID: &str = "com.pais.handy";
const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const SETUP_BUDGET: Duration = Duration::from_secs(90);
const SEND_BUDGET: Duration = Duration::from_secs(5);
const SESSION_CLOSE_BUDGET: Duration = Duration::from_millis(500);
const CONNECTION_CLOSE_BUDGET: Duration = Duration::from_secs(1);
const KEY_STEP_DELAY: Duration = Duration::from_millis(15);

const CONTROL_L: i32 = 0xffe3;
const SHIFT_L: i32 = 0xffe1;
const SUPER_L: i32 = 0xffeb;
const INSERT: i32 = 0xff63;
const RETURN: i32 = 0xff0d;
const V: i32 = 0x0076;

const NOT_READY: &str =
    "Native Wayland pasting is not ready. Open Handy and choose Set Up Pasting, then retry.";
const CANCELLED: &str = "Native Wayland input was stopped. No further keys will be sent.";

static WORKER: OnceLock<Result<Worker, String>> = OnceLock::new();
static AUTOMATIC_SETUP: AutomaticSetup = AutomaticSetup::new();

/// One automatic request per Portal activation. Cancellation and explicit Stop
/// keep it suppressed; only leaving the backend resets this policy.
struct AutomaticSetup(AtomicBool);

impl AutomaticSetup {
    const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    fn pending(&self, method: PasteMethod) -> bool {
        matches!(
            method,
            PasteMethod::CtrlV | PasteMethod::CtrlShiftV | PasteMethod::ShiftInsert
        ) && !self.0.load(Ordering::SeqCst)
    }

    fn claim(&self, method: PasteMethod) -> bool {
        self.pending(method) && !self.0.swap(true, Ordering::SeqCst)
    }

    fn suppress(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

enum Operation {
    Setup,
    Chord(Vec<i32>),
    Shutdown,
}

struct Request {
    operation: Operation,
    generation: u64,
    response: oneshot::Sender<Result<(), String>>,
}

struct Worker {
    requests: mpsc::UnboundedSender<Request>,
    cancellation: watch::Sender<u64>,
    generation: Arc<AtomicU64>,
    ready: Arc<AtomicBool>,
    stopping: AtomicBool,
    // Reject concurrent work rather than queueing a paste behind a consent
    // dialog, another paste, or an expired clipboard/focus context.
    operation_lock: Mutex<()>,
}

impl Worker {
    fn new() -> Result<Self, String> {
        let (requests, receiver) = mpsc::unbounded_channel();
        let (cancellation, cancelled) = watch::channel(0);
        let generation = Arc::new(AtomicU64::new(0));
        let ready = Arc::new(AtomicBool::new(false));
        let actor_ready = ready.clone();
        std::thread::Builder::new()
            .name("handy-portal-input".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(runtime) = runtime {
                    runtime.block_on(run_actor(receiver, cancelled, actor_ready));
                }
            })
            .map_err(|_| "Cannot start the native Wayland input worker.".to_owned())?;
        Ok(Self {
            requests,
            cancellation,
            generation,
            ready,
            stopping: AtomicBool::new(false),
            operation_lock: Mutex::new(()),
        })
    }

    fn call(&self, operation: Operation) -> Result<(), String> {
        // Snapshot before checking stopping. If shutdown races this call after
        // the check, the old generation still makes the actor reject it.
        let generation = self.generation.load(Ordering::SeqCst);
        self.call_at_generation(operation, generation)
    }

    fn call_at_generation(&self, operation: Operation, generation: u64) -> Result<(), String> {
        let _guard = self.operation_lock.try_lock().map_err(|_| {
            "Native Wayland input is busy. Wait for setup or the current paste to finish."
                .to_owned()
        })?;
        if self.stopping.load(Ordering::SeqCst) {
            return Err("Native Wayland input is stopping. Wait, then retry setup.".into());
        }
        if generation != self.generation.load(Ordering::SeqCst) {
            return Err(CANCELLED.into());
        }
        if matches!(operation, Operation::Chord(_)) && !self.ready.load(Ordering::SeqCst) {
            return Err(NOT_READY.into());
        }
        self.send(operation, generation)
    }

    fn send(&self, operation: Operation, generation: u64) -> Result<(), String> {
        let (response, result) = oneshot::channel();
        self.requests
            .send(Request {
                operation,
                generation,
                response,
            })
            .map_err(|_| "The native Wayland input worker stopped. Restart Handy.".to_owned())?;
        // The actor owns deadlines and completes cancellation/cleanup before
        // replying. An independent caller timeout could leave late key input.
        result
            .blocking_recv()
            .map_err(|_| "The native Wayland input worker stopped. Restart Handy.".to_owned())?
    }
}

fn worker() -> Result<&'static Worker, String> {
    WORKER
        .get_or_init(Worker::new)
        .as_ref()
        .map_err(Clone::clone)
}

/// Ask for a keyboard-only, nonpersistent portal session after an explicit
/// Set Up Pasting request. Call on a blocking worker thread, with recording and
/// output held idle until consent closes.
pub fn setup() -> Result<(), String> {
    AUTOMATIC_SETUP.suppress();
    worker()?.call(Operation::Setup)
}

pub fn automatic_setup_pending(method: PasteMethod) -> bool {
    AUTOMATIC_SETUP.pending(method)
}

/// Request consent once at startup/backend activation, before shortcuts become
/// ready. Stop can invalidate the request even before it reaches the worker.
pub fn setup_automatically(method: PasteMethod) -> Result<(), String> {
    if !AUTOMATIC_SETUP.pending(method) {
        return Ok(());
    }
    let service = worker()?;
    let generation = service.generation.load(Ordering::SeqCst);
    if !AUTOMATIC_SETUP.claim(method) {
        return Ok(());
    }
    let result = service.call_at_generation(Operation::Setup, generation);
    // Stop or backend exit already canceled this automatic request; do not
    // replace that deliberate action with a permission-error notice.
    if generation != service.generation.load(Ordering::SeqCst) {
        return Ok(());
    }
    result
}

pub fn reset_automatic_setup() {
    AUTOMATIC_SETUP.reset();
}

/// Send an approved clipboard-paste chord without showing any permission UI.
/// The caller must already own the clipboard and serialize the complete paste.
pub fn send_paste(method: PasteMethod) -> Result<(), String> {
    let chord = paste_chord(method)?;
    let service = WORKER
        .get()
        .and_then(|result| result.as_ref().ok())
        .ok_or(NOT_READY)?;
    service.call(Operation::Chord(chord))?;
    log::info!("Native Wayland paste chord accepted: {method:?}");
    Ok(())
}

/// Submit using the same approved native input session as the preceding paste.
pub fn send_submit(key: AutoSubmitKey) -> Result<(), String> {
    let service = WORKER
        .get()
        .and_then(|result| result.as_ref().ok())
        .ok_or(NOT_READY)?;
    service.call(Operation::Chord(submit_chord(key)))
}

/// Immediately cancel pending work and close the input session, including its
/// consent UI. Idempotent; does not initialize a worker or acquire permission.
/// Wait for this on a blocking worker before reporting Stop Pasting complete.
pub fn shutdown() -> Result<(), String> {
    AUTOMATIC_SETUP.suppress();
    let Some(Ok(service)) = WORKER.get() else {
        return Ok(());
    };
    if service.stopping.swap(true, Ordering::SeqCst) {
        return Err("Native Wayland input is already stopping. Wait, then retry.".into());
    }
    service.ready.store(false, Ordering::SeqCst);
    let generation = service.generation.fetch_add(1, Ordering::SeqCst) + 1;
    service.cancellation.send_replace(generation);
    let result = service.send(Operation::Shutdown, generation);
    service.stopping.store(false, Ordering::SeqCst);
    result
}

fn paste_chord(method: PasteMethod) -> Result<Vec<i32>, String> {
    match method {
        PasteMethod::CtrlV => Ok(vec![CONTROL_L, V]),
        PasteMethod::CtrlShiftV => Ok(vec![CONTROL_L, SHIFT_L, V]),
        PasteMethod::ShiftInsert => Ok(vec![SHIFT_L, INSERT]),
        _ => Err("This paste method does not use native Wayland keyboard input.".into()),
    }
}

fn submit_chord(key: AutoSubmitKey) -> Vec<i32> {
    match key {
        AutoSubmitKey::Enter => vec![RETURN],
        AutoSubmitKey::CtrlEnter => vec![CONTROL_L, RETURN],
        AutoSubmitKey::CmdEnter => vec![SUPER_L, RETURN],
    }
}

#[derive(Default)]
struct PortalState {
    connection: Option<Connection>,
    proxy: Option<RemoteDesktop>,
    session: Option<Arc<Session<RemoteDesktop>>>,
    watcher: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
    invalidated: Arc<Notify>,
    // Mark BEFORE awaiting a press: a failed D-Bus response can still mean the
    // compositor received key-down. Cleanup releases every possibly held key.
    pressed: Vec<i32>,
}

impl PortalState {
    fn is_ready(&self) -> bool {
        self.session.is_some() && self.alive.load(Ordering::SeqCst)
    }

    async fn setup(&mut self, ready: Arc<AtomicBool>) -> Result<(), String> {
        if self.is_ready() {
            return Ok(());
        }
        self.close().await?;
        let connection = Connection::session()
            .await
            .map_err(|_| "Cannot connect to the desktop session bus.".to_owned())?;
        self.connection = Some(connection.clone());
        let app_id = APP_ID
            .parse::<ashpd::AppID>()
            .map_err(|_| "Handy's desktop application ID is invalid.".to_owned())?;
        ashpd::register_host_app_with_connection(connection.clone(), app_id)
            .await
            .map_err(|_| {
                "Cannot identify Handy to the desktop portal. Install com.pais.handy.desktop in your user applications directory."
                    .to_owned()
            })?;
        let bus = DBusProxy::new(&connection)
            .await
            .map_err(|_| "Cannot identify the desktop portal service.".to_owned())?;
        let portal_name = PORTAL_BUS_NAME
            .try_into()
            .map_err(|_| "The desktop portal service name is invalid.".to_owned())?;
        let owner = bus
            .get_name_owner(portal_name)
            .await
            .map_err(|_| "Cannot identify the desktop portal service.".to_owned())?
            .to_string();
        let proxy = RemoteDesktop::with_connection(connection.clone())
            .await
            .map_err(|_| "Cannot connect to the Remote Desktop portal.".to_owned())?;
        self.proxy = Some(proxy);
        let proxy = self.proxy.as_ref().ok_or(NOT_READY)?;
        let session = Arc::new(
            proxy
                .create_session(Default::default())
                .await
                .map_err(|_| "Cannot create a keyboard-only desktop portal session.".to_owned())?,
        );
        self.session = Some(session.clone());
        self.alive.store(true, Ordering::SeqCst);
        let (watch_ready, watching) = oneshot::channel();
        self.watcher = Some(tokio::spawn(watch_session(
            connection,
            session.clone(),
            self.alive.clone(),
            ready.clone(),
            self.invalidated.clone(),
            watch_ready,
            owner,
        )));
        watching
            .await
            .map_err(|_| "Cannot watch the native input permission session.".to_owned())??;
        proxy
            .select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(Some(DeviceType::Keyboard.into()))
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .map_err(|_| "The desktop refused keyboard-only input selection.".to_owned())?
            .response()
            .map_err(|_| "The desktop refused keyboard-only input selection.".to_owned())?;
        let approval = proxy
            .start(&session, None, Default::default())
            .await
            .map_err(|_| "Native input setup did not complete. Retry Set Up Pasting.".to_owned())?
            .response()
            .map_err(|_| {
                "Native input permission was not granted. Retry Set Up Pasting.".to_owned()
            })?;
        if approval.devices() != DeviceType::Keyboard
            || !approval.streams().is_empty()
            || approval.is_clipboard_enabled()
        {
            return Err(
                "The desktop did not grant exactly keyboard-only input. Permission was closed."
                    .into(),
            );
        }
        ready.store(true, Ordering::SeqCst);
        if !self.is_ready() {
            ready.store(false, Ordering::SeqCst);
            return Err(NOT_READY.into());
        }
        log::info!("Native Wayland paste permission ready (keyboard only, not persisted)");
        Ok(())
    }

    async fn chord(&mut self, keys: &[i32]) -> Result<(), String> {
        if !self.is_ready() {
            return Err(NOT_READY.into());
        }
        for &key in keys {
            if !self.is_ready() {
                return Err(NOT_READY.into());
            }
            self.pressed.push(key);
            self.key(key, KeyState::Pressed).await?;
            tokio::time::sleep(KEY_STEP_DELAY).await;
        }
        while let Some(&key) = self.pressed.last() {
            self.key(key, KeyState::Released).await?;
            self.pressed.pop();
            tokio::time::sleep(KEY_STEP_DELAY).await;
        }
        Ok(())
    }

    async fn key(&self, key: i32, state: KeyState) -> Result<(), String> {
        let proxy = self.proxy.as_ref().ok_or(NOT_READY)?;
        let session = self.session.as_ref().ok_or(NOT_READY)?;
        proxy
            .notify_keyboard_keysym(session, key, state, Default::default())
            .await
            .map_err(|_| "Native keyboard input failed. Retry Set Up Pasting.".to_owned())
    }

    async fn close(&mut self) -> Result<(), String> {
        self.alive.store(false, Ordering::SeqCst);
        // Bound the whole release sequence, not each key independently.
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            while let Some(&key) = self.pressed.last() {
                let _ = self.key(key, KeyState::Released).await;
                self.pressed.pop();
            }
        })
        .await;
        self.pressed.clear();
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
            let _ = watcher.await;
        }
        let session = self.session.take();
        let connection = self.connection.take();
        let no_session = session.is_none() && connection.is_none();
        let session_closed = if let Some(session) = session.as_ref() {
            matches!(
                tokio::time::timeout(SESSION_CLOSE_BUDGET, session.close()).await,
                Ok(Ok(()))
            )
        } else {
            false
        };
        self.proxy = None;
        let connection_closed = if let Some(connection) = connection.as_ref() {
            matches!(
                tokio::time::timeout(CONNECTION_CLOSE_BUDGET, connection.clone().close()).await,
                Ok(Ok(()))
            )
        } else {
            false
        };
        if no_session || session_closed || connection_closed {
            Ok(())
        } else {
            // Retain handles only for another close attempt. They stay unusable
            // for input, and setup must close them before opening a new grant.
            self.session = session;
            self.connection = connection;
            Err("Could not confirm that the desktop input permission closed. Use GNOME's top-bar Stop Screen Sharing button, then retry Stop Pasting.".into())
        }
    }
}

async fn watch_session(
    connection: Connection,
    session: Arc<Session<RemoteDesktop>>,
    alive: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    invalidated: Arc<Notify>,
    started: oneshot::Sender<Result<(), String>>,
    expected_owner: String,
) {
    let watches = async {
        let bus = DBusProxy::new(&connection)
            .await
            .map_err(|_| "Cannot watch the desktop portal service.".to_owned())?;
        let mut owners = bus
            .receive_name_owner_changed_with_args(&[(0, PORTAL_BUS_NAME)])
            .await
            .map_err(|_| "Cannot watch the desktop portal service.".to_owned())?;
        let mut closed = session
            .receive_closed()
            .await
            .map_err(|_| "Cannot watch the native input permission session.".to_owned())?;
        let portal_name = PORTAL_BUS_NAME
            .try_into()
            .map_err(|_| "The desktop portal service name is invalid.".to_owned())?;
        let owner = bus
            .get_name_owner(portal_name)
            .await
            .map_err(|_| "Cannot identify the desktop portal service.".to_owned())?;
        if owner.as_str() != expected_owner {
            return Err("The desktop portal restarted. Retry Set Up Pasting.".into());
        }
        let _ = started.send(Ok(()));
        tokio::select! {
            _ = closed.next() => {},
            _ = owners.next() => {},
        }
        Ok::<(), String>(())
    };
    // Dropping the acknowledgement on setup failure makes setup fail closed.
    let _ = watches.await;
    alive.store(false, Ordering::SeqCst);
    ready.store(false, Ordering::SeqCst);
    invalidated.notify_waiters();
}

async fn run_actor(
    mut requests: mpsc::UnboundedReceiver<Request>,
    mut cancelled: watch::Receiver<u64>,
    ready: Arc<AtomicBool>,
) {
    let mut state = PortalState::default();
    loop {
        let invalidated = state.invalidated.clone();
        let request = tokio::select! {
            biased;
            result = cancelled.changed() => {
                if result.is_err() { break; }
                ready.store(false, Ordering::SeqCst);
                let _ = state.close().await;
                continue;
            }
            _ = invalidated.notified() => {
                ready.store(false, Ordering::SeqCst);
                let _ = state.close().await;
                continue;
            }
            request = requests.recv() => match request {
                Some(request) => request,
                None => break,
            },
        };
        let Request {
            operation,
            generation,
            mut response,
        } = request;
        if response.is_closed() {
            continue;
        }
        if generation != *cancelled.borrow() {
            let _ = response.send(Err(CANCELLED.into()));
            continue;
        }
        if matches!(operation, Operation::Shutdown) {
            ready.store(false, Ordering::SeqCst);
            let result = state.close().await;
            let _ = response.send(result);
            continue;
        }
        let budget = match &operation {
            Operation::Setup => SETUP_BUDGET,
            Operation::Chord(_) => SEND_BUDGET,
            Operation::Shutdown => unreachable!(),
        };
        let invalidated = state.invalidated.clone();
        let operation = async {
            match operation {
                Operation::Setup => state.setup(ready.clone()).await,
                Operation::Chord(keys) => state.chord(&keys).await,
                Operation::Shutdown => unreachable!(),
            }
        };
        let mut result = tokio::select! {
            biased;
            _ = cancelled.changed() => Err(CANCELLED.into()),
            _ = response.closed() => Err(CANCELLED.into()),
            _ = invalidated.notified() => Err(NOT_READY.into()),
            result = tokio::time::timeout(budget, operation) => result.unwrap_or_else(|_| {
                Err("Native Wayland input timed out and permission was closed. Retry Set Up Pasting.".into())
            }),
        };
        if result.is_err() {
            ready.store(false, Ordering::SeqCst);
            if let Err(cleanup_error) = state.close().await {
                if let Err(error) = &mut result {
                    error.push(' ');
                    error.push_str(&cleanup_error);
                }
            }
        }
        let _ = response.send(result);
    }
    ready.store(false, Ordering::SeqCst);
    let _ = state.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_setup_is_only_for_clipboard_methods_and_does_not_repeat() {
        let policy = AutomaticSetup::new();
        for method in [
            PasteMethod::None,
            PasteMethod::Direct,
            PasteMethod::ExternalScript,
        ] {
            assert!(!policy.claim(method));
        }
        assert!(policy.claim(PasteMethod::ShiftInsert));
        // Success, denial and timeout all consume this activation's attempt.
        assert!(!policy.claim(PasteMethod::CtrlV));
        assert!(!policy.claim(PasteMethod::CtrlShiftV));
        policy.reset();
        assert!(policy.claim(PasteMethod::CtrlShiftV));
    }

    #[test]
    fn stop_before_worker_creation_suppresses_automatic_permission() {
        let policy = AutomaticSetup::new();
        policy.suppress();
        assert!(!policy.pending(PasteMethod::ShiftInsert));
        assert!(!policy.claim(PasteMethod::ShiftInsert));
    }

    #[test]
    fn automatic_setup_with_pre_stop_generation_is_rejected_before_queueing() {
        let (requests, mut receiver) = mpsc::unbounded_channel();
        let (cancellation, _) = watch::channel(1);
        let service = Worker {
            requests,
            cancellation,
            generation: Arc::new(AtomicU64::new(1)),
            ready: Arc::new(AtomicBool::new(false)),
            stopping: AtomicBool::new(false),
            operation_lock: Mutex::new(()),
        };
        assert_eq!(
            service.call_at_generation(Operation::Setup, 0),
            Err(CANCELLED.into())
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn clipboard_chords_use_keysyms_and_keep_modifiers_before_the_main_key() {
        assert_eq!(paste_chord(PasteMethod::CtrlV).unwrap(), [CONTROL_L, V]);
        assert_eq!(
            paste_chord(PasteMethod::CtrlShiftV).unwrap(),
            [CONTROL_L, SHIFT_L, V]
        );
        assert_eq!(
            paste_chord(PasteMethod::ShiftInsert).unwrap(),
            [SHIFT_L, INSERT]
        );
        assert!(paste_chord(PasteMethod::Direct).is_err());
        assert!(paste_chord(PasteMethod::None).is_err());
        assert!(paste_chord(PasteMethod::ExternalScript).is_err());
    }

    #[test]
    fn submission_has_an_explicit_native_chord_for_each_setting() {
        assert_eq!(submit_chord(AutoSubmitKey::Enter), [RETURN]);
        assert_eq!(submit_chord(AutoSubmitKey::CtrlEnter), [CONTROL_L, RETURN]);
        assert_eq!(submit_chord(AutoSubmitKey::CmdEnter), [SUPER_L, RETURN]);
    }

    #[test]
    fn paste_before_setup_is_rejected_without_queueing_work() {
        let (requests, mut receiver) = mpsc::unbounded_channel();
        let (cancellation, _) = watch::channel(0);
        let service = Worker {
            requests,
            cancellation,
            generation: Arc::new(AtomicU64::new(0)),
            ready: Arc::new(AtomicBool::new(false)),
            stopping: AtomicBool::new(false),
            operation_lock: Mutex::new(()),
        };
        assert_eq!(
            service.call(Operation::Chord(vec![CONTROL_L, V])),
            Err(NOT_READY.into())
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn paste_cannot_wait_behind_setup_or_another_output() {
        let (requests, mut receiver) = mpsc::unbounded_channel();
        let (cancellation, _) = watch::channel(0);
        let service = Worker {
            requests,
            cancellation,
            generation: Arc::new(AtomicU64::new(0)),
            ready: Arc::new(AtomicBool::new(true)),
            stopping: AtomicBool::new(false),
            operation_lock: Mutex::new(()),
        };
        let _setup_in_progress = service.operation_lock.lock().unwrap();
        let result = service.call(Operation::Chord(vec![CONTROL_L, V]));
        assert!(result.unwrap_err().contains("busy"));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn queued_setup_from_before_shutdown_is_rejected_before_portal_access() {
        let (requests, receiver) = mpsc::unbounded_channel();
        let (_cancellation, cancelled) = watch::channel(1);
        let ready = Arc::new(AtomicBool::new(false));
        let actor = tokio::spawn(run_actor(receiver, cancelled, ready));
        let (response, result) = oneshot::channel();
        requests
            .send(Request {
                operation: Operation::Setup,
                generation: 0,
                response,
            })
            .unwrap();
        assert_eq!(result.await.unwrap(), Err(CANCELLED.into()));
        drop(requests);
        actor.await.unwrap();
    }

    #[tokio::test]
    async fn actor_cannot_create_permission_for_a_paste_when_readiness_races() {
        let (requests, receiver) = mpsc::unbounded_channel();
        let (_cancellation, cancelled) = watch::channel(0);
        let ready = Arc::new(AtomicBool::new(true));
        let actor = tokio::spawn(run_actor(receiver, cancelled, ready.clone()));
        let (response, result) = oneshot::channel();
        requests
            .send(Request {
                operation: Operation::Chord(vec![CONTROL_L, V]),
                generation: 0,
                response,
            })
            .unwrap();
        assert_eq!(result.await.unwrap(), Err(NOT_READY.into()));
        assert!(!ready.load(Ordering::SeqCst));
        let (response, result) = oneshot::channel();
        requests
            .send(Request {
                operation: Operation::Shutdown,
                generation: 0,
                response,
            })
            .unwrap();
        assert_eq!(result.await.unwrap(), Ok(()));
        drop(requests);
        actor.await.unwrap();
    }

    #[tokio::test]
    async fn an_abandoned_setup_request_is_skipped_before_following_shutdown() {
        let (requests, receiver) = mpsc::unbounded_channel();
        let (_cancellation, cancelled) = watch::channel(0);
        let ready = Arc::new(AtomicBool::new(false));
        let (response, abandoned) = oneshot::channel();
        drop(abandoned);
        requests
            .send(Request {
                operation: Operation::Setup,
                generation: 0,
                response,
            })
            .unwrap();
        let (response, result) = oneshot::channel();
        requests
            .send(Request {
                operation: Operation::Shutdown,
                generation: 0,
                response,
            })
            .unwrap();
        drop(requests);
        run_actor(receiver, cancelled, ready.clone()).await;
        assert_eq!(result.await.unwrap(), Ok(()));
        assert!(!ready.load(Ordering::SeqCst));
    }
}
