//! Wayland shortcuts mediated by the GlobalShortcuts portal.
//!
//! All portal work lives on a dedicated runtime. Each binding generation owns a
//! separate D-Bus connection and calls BindShortcuts exactly once. No keyboard
//! devices are opened and no fallback backend is selected on failure.

use ashpd::desktop::global_shortcuts::{
    Activated, Deactivated, GlobalShortcuts, NewShortcut, ShortcutsChanged,
};
use ashpd::desktop::Session;
use ashpd::zbus::{fdo::DBusProxy, message::Type, Connection, MatchRule, Message, MessageStream};
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use log::{error, info, warn};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc as response_channel, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, oneshot, watch};

use crate::managers::audio::AudioRecordingManager;
use crate::managers::transcription::TranscriptionManager;
use crate::notices::{self, Notice, NoticeCode, NoticeSource};
use crate::settings::{self, ShortcutBinding};

use super::handler::handle_shortcut_event;

const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const BIND_TIMEOUT: Duration = Duration::from_secs(90);
const RESTORE_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_TIMEOUT: Duration = Duration::from_secs(135);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
type Bindings = BTreeMap<String, ShortcutBinding>;
type Response = response_channel::Sender<Result<(), String>>;

/// Keep the state allocation alive while concurrent Tauri commands hold guards.
#[derive(Default)]
pub struct PortalState {
    actor: Mutex<Option<ActorHandle>>,
}

struct ActorHandle {
    commands: mpsc::UnboundedSender<Command>,
    shutdown: watch::Sender<bool>,
    finished: response_channel::Receiver<()>,
    thread: JoinHandle<()>,
}

struct Command {
    operation: Operation,
    response: Response,
    cancelled: watch::Receiver<bool>,
}

enum Operation {
    Initialize(Bindings),
    Register(ShortcutBinding),
    Unregister(String),
    Suspend,
    Resume,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionKey {
    generation: u64,
    path: String,
}

#[derive(Default)]
struct DispatchState {
    active: Option<SessionKey>,
    bindings: Bindings,
    held: BTreeSet<String>,
}

impl DispatchState {
    fn install(&mut self, key: SessionKey, bindings: Bindings) {
        self.active = Some(key);
        self.bindings = bindings;
        self.held.clear();
    }

    /// Reject stale sessions, unknown IDs, autorepeat and duplicate releases.
    fn event(
        &mut self,
        key: &SessionKey,
        path: &str,
        id: &str,
        pressed: bool,
    ) -> Option<ShortcutBinding> {
        if self.active.as_ref() != Some(key) || key.path != path {
            return None;
        }
        let binding = self.bindings.get(id)?;
        let changed = if pressed {
            self.held.insert(id.to_owned())
        } else {
            self.held.remove(id)
        };
        changed.then(|| binding.clone())
    }

    /// Return whether this generation owned any pressed shortcut.
    fn clear(&mut self, key: Option<&SessionKey>) -> bool {
        if key.is_some() && self.active.as_ref() != key {
            return false;
        }
        let held = !self.held.is_empty();
        self.active = None;
        self.bindings.clear();
        self.held.clear();
        held
    }
}

fn managed_state(app: &AppHandle) -> tauri::State<'_, PortalState> {
    if app.try_state::<PortalState>().is_none() {
        app.manage(PortalState::default());
    }
    app.state::<PortalState>()
}

fn initial_bindings(app: &AppHandle) -> Result<Bindings, String> {
    let current = settings::get_settings(app);
    let defaults = settings::get_default_settings();
    let mut bindings = Bindings::new();
    for id in ["transcribe", "transcribe_with_post_process"] {
        if id == "transcribe_with_post_process" && !current.post_process_enabled {
            continue;
        }
        if let Some(binding) = current
            .bindings
            .get(id)
            .or_else(|| defaults.bindings.get(id))
        {
            validate_shortcut(&binding.current_binding)?;
            bindings.insert(id.to_owned(), binding.clone());
        }
    }
    if !bindings.contains_key("transcribe") {
        return Err("No transcription shortcut is configured for the portal.".into());
    }
    Ok(bindings)
}

fn ensure_actor(app: &AppHandle) -> Result<(), String> {
    let state = managed_state(app);
    let mut slot = state
        .actor
        .lock()
        .map_err(|_| "Portal state lock poisoned")?;
    if slot
        .as_ref()
        .is_some_and(|actor| !actor.thread.is_finished())
    {
        return Ok(());
    }
    if let Some(old) = slot.take() {
        let _ = old.shutdown.send(true);
        let _ = old.thread.join();
    }
    let (commands, receiver) = mpsc::unbounded_channel();
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let (finished_tx, finished) = response_channel::channel();
    let app = app.clone();
    let thread = std::thread::Builder::new()
        .name("handy-portal-shortcuts".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(actor_loop(app, receiver, shutdown_receiver)),
                Err(e) => {
                    report_failure(&app, &format!("Cannot start shortcut portal runtime: {e}"))
                }
            }
            let _ = finished_tx.send(());
        })
        .map_err(|e| format!("Cannot start shortcut portal thread: {e}"))?;
    *slot = Some(ActorHandle {
        commands,
        shutdown,
        finished,
        thread,
    });
    Ok(())
}

fn request(app: &AppHandle, operation: Operation) -> Result<(), String> {
    let state = managed_state(app);
    let commands = {
        let slot = state
            .actor
            .lock()
            .map_err(|_| "Portal state lock poisoned")?;
        slot.as_ref()
            .ok_or("Shortcut portal is not initialized. Select Portal again to retry.")?
            .commands
            .clone()
    };
    let (response, receiver) = response_channel::channel();
    let (cancel, cancelled) = watch::channel(false);
    commands
        .send(Command {
            operation,
            response,
            cancelled,
        })
        .map_err(|_| "Shortcut portal stopped. Select Portal again to retry.".to_string())?;
    match receiver.recv_timeout(RPC_TIMEOUT) {
        Ok(result) => result,
        Err(response_channel::RecvTimeoutError::Timeout) => {
            let _ = cancel.send(true);
            Err(
                "Shortcut portal did not respond in time; the pending request was cancelled."
                    .into(),
            )
        }
        Err(response_channel::RecvTimeoutError::Disconnected) => {
            Err("Shortcut portal stopped before answering the request.".into())
        }
    }
}

/// Wait for genuine registration/consent success; the caller must not be on GTK's thread.
pub fn init_shortcuts(app: &AppHandle) -> Result<(), String> {
    let bindings = initial_bindings(app)?;
    ensure_actor(app)?;
    request(app, Operation::Initialize(bindings))
}

pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    if binding.id == "cancel" {
        return Ok(());
    }
    if !matches!(
        binding.id.as_str(),
        "transcribe" | "transcribe_with_post_process"
    ) {
        return Err("This action is not supported by the shortcut portal.".into());
    }
    validate_shortcut(&binding.current_binding)?;
    request(app, Operation::Register(binding))
}

pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    if binding.id == "cancel" {
        return Ok(());
    }
    request(app, Operation::Unregister(binding.id))
}

pub fn suspend_shortcuts(app: &AppHandle) -> Result<(), String> {
    request(app, Operation::Suspend)
}

pub fn resume_shortcuts(app: &AppHandle) -> Result<(), String> {
    request(app, Operation::Resume)
}

/// Interrupt consent waits directly; do not wait for them to leave the command queue.
pub fn cleanup_shortcuts(app: &AppHandle) {
    let Some(state) = app.try_state::<PortalState>() else {
        return;
    };
    let actor = match state.actor.lock() {
        Ok(mut slot) => slot.take(),
        Err(_) => {
            error!("Cannot lock shortcut portal during shutdown");
            return;
        }
    };
    if let Some(actor) = actor {
        let _ = actor.shutdown.send(true);
        if actor.finished.recv_timeout(SHUTDOWN_TIMEOUT).is_ok() || actor.thread.is_finished() {
            let _ = actor.thread.join();
        } else {
            warn!("Shortcut portal shutdown is still finishing in the background");
        }
    }
}

async fn wait_cancelled(receiver: &mut watch::Receiver<bool>) {
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            // Dropping a completed command's sender is not cancellation.
            futures_util::future::pending::<()>().await;
        }
    }
}

struct Actor {
    app: AppHandle,
    bindings: Bindings,
    active: Option<BoundSession>,
    suspended: bool,
    generation: u64,
    dispatch: Arc<Mutex<DispatchState>>,
    shutdown: watch::Receiver<bool>,
}

async fn actor_loop(
    app: AppHandle,
    mut commands: mpsc::UnboundedReceiver<Command>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut actor = Actor {
        app,
        bindings: Bindings::new(),
        active: None,
        suspended: false,
        generation: 0,
        dispatch: Arc::new(Mutex::new(DispatchState::default())),
        shutdown: shutdown.clone(),
    };
    loop {
        let command = tokio::select! {
            biased;
            _ = wait_cancelled(&mut shutdown) => break,
            command = commands.recv() => match command { Some(c) => c, None => break },
        };
        if *command.cancelled.borrow() {
            let _ = command
                .response
                .send(Err("Shortcut request was cancelled.".into()));
            continue;
        }
        let result = actor.apply(command.operation, command.cancelled).await;
        if result.is_ok() {
            notices::clear(&actor.app, NoticeSource::Shortcuts);
        }
        let _ = command.response.send(result);
    }
    actor.stop_active().await;
    info!("Shortcut portal stopped");
}

impl Actor {
    async fn stop_active(&mut self) {
        let (owned, held) = self
            .dispatch
            .lock()
            .map(|mut state| {
                let owned = state.active.is_some();
                (owned, state.clear(None))
            })
            .unwrap_or((false, false));
        cancel_owned_operation(&self.app, owned, held);
        if let Some(active) = self.active.take() {
            active.close().await;
        }
    }

    async fn start(
        &mut self,
        bindings: Bindings,
        budget: Duration,
        cancelled: watch::Receiver<bool>,
    ) -> Result<Option<BoundSession>, String> {
        if bindings.is_empty() {
            return Ok(None);
        }
        self.generation = self.generation.wrapping_add(1);
        start_session(
            self.app.clone(),
            bindings,
            self.generation,
            self.dispatch.clone(),
            self.shutdown.clone(),
            cancelled,
            budget,
        )
        .await
        .map(Some)
    }

    async fn replace(
        &mut self,
        desired: Bindings,
        cancelled: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let old = self.bindings.clone();
        let restore = self.active.is_some();
        self.stop_active().await;
        match self
            .start(desired.clone(), BIND_TIMEOUT, cancelled.clone())
            .await
        {
            Ok(active) => {
                self.bindings = desired;
                self.active = active;
                Ok(())
            }
            Err(error) => {
                if restore && !*self.shutdown.borrow() && !*cancelled.borrow() {
                    match self.start(old, RESTORE_TIMEOUT, cancelled).await {
                        Ok(active) => {
                            self.active = active;
                            Err(format!(
                                "{error} The previous shortcut registration was restored."
                            ))
                        }
                        Err(restore_error) => {
                            let detail = format!(
                                "{error} Previous shortcuts could not be restored: {restore_error}"
                            );
                            report_failure(&self.app, &detail);
                            Err(detail)
                        }
                    }
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn apply(
        &mut self,
        operation: Operation,
        cancelled: watch::Receiver<bool>,
    ) -> Result<(), String> {
        match operation {
            Operation::Initialize(bindings) => {
                self.suspended = false;
                self.replace(bindings, cancelled).await
            }
            Operation::Register(binding) => {
                let unchanged = self.bindings.get(&binding.id).is_some_and(|existing| {
                    existing.current_binding == binding.current_binding
                        && existing.name == binding.name
                });
                if unchanged
                    && (self.suspended
                        || self
                            .active
                            .as_ref()
                            .is_some_and(|s| !s.worker.is_finished()))
                {
                    return Ok(());
                }
                let mut next = self.bindings.clone();
                next.insert(binding.id.clone(), binding);
                if self.suspended {
                    self.bindings = next;
                    Ok(())
                } else {
                    self.replace(next, cancelled).await
                }
            }
            Operation::Unregister(id) => {
                if !self.bindings.contains_key(&id) {
                    return Ok(());
                }
                let mut next = self.bindings.clone();
                next.remove(&id);
                if self.suspended {
                    self.bindings = next;
                    Ok(())
                } else {
                    self.replace(next, cancelled).await
                }
            }
            Operation::Suspend => {
                self.suspended = true;
                self.stop_active().await;
                Ok(())
            }
            Operation::Resume => {
                if !self.suspended
                    && self
                        .active
                        .as_ref()
                        .is_some_and(|s| !s.worker.is_finished())
                {
                    return Ok(());
                }
                // Captured settings have already been written. On denial leave
                // them intact and fail visibly, rather than use a hidden old chord.
                match self
                    .start(self.bindings.clone(), BIND_TIMEOUT, cancelled)
                    .await
                {
                    Ok(active) => {
                        self.active = active;
                        self.suspended = false;
                        Ok(())
                    }
                    Err(error) => {
                        report_failure(&self.app, &error);
                        Err(error)
                    }
                }
            }
        }
    }
}

struct BoundSession {
    stop: watch::Sender<bool>,
    worker: tokio::task::JoinHandle<()>,
    connection: Connection,
}

impl BoundSession {
    async fn close(mut self) {
        let _ = self.stop.send(true);
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, self.connection.close()).await;
        if tokio::time::timeout(CLOSE_TIMEOUT, &mut self.worker)
            .await
            .is_err()
        {
            self.worker.abort();
        }
    }
}

struct Prepared {
    proxy: GlobalShortcuts,
    session: Session<GlobalShortcuts>,
    key: SessionKey,
    owner: String,
    signals: BoxStream<'static, ashpd::zbus::Result<Message>>,
}

async fn start_session(
    app: AppHandle,
    bindings: Bindings,
    generation: u64,
    dispatch: Arc<Mutex<DispatchState>>,
    shutdown: watch::Receiver<bool>,
    cancelled: watch::Receiver<bool>,
    budget: Duration,
) -> Result<BoundSession, String> {
    let (stop, local_stop) = watch::channel(false);
    let (ready, response) = oneshot::channel();
    let worker = tokio::spawn(session_worker(
        app, bindings, generation, dispatch, shutdown, cancelled, local_stop, ready, budget,
    ));
    match response.await {
        Ok(Ok(connection)) => Ok(BoundSession {
            stop,
            worker,
            connection,
        }),
        Ok(Err(error)) => {
            let _ = worker.await;
            Err(error)
        }
        Err(_) => {
            let _ = stop.send(true);
            worker.abort();
            Err("Shortcut portal session stopped during initialization.".into())
        }
    }
}

async fn prepare(
    connection: &Connection,
    app_id: &str,
    bindings: &Bindings,
    generation: u64,
) -> Result<Prepared, String> {
    let id = app_id
        .parse::<ashpd::AppID>()
        .map_err(|_| "Handy's application ID is invalid for desktop portals.".to_string())?;
    ashpd::register_host_app_with_connection(connection.clone(), id).await.map_err(|e| format!(
        "Cannot register Handy with the desktop portal. Ensure {app_id}.desktop is installed in your user applications directory. {}", safe_error(&e.to_string())))?;
    info!("Shortcut portal host application registered: {app_id}");
    let proxy = GlobalShortcuts::with_connection(connection.clone())
        .await
        .map_err(|e| portal_error("connect", e))?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|e| portal_error("create session", e))?;
    let path = serde_json::to_value(&session)
        .map_err(|_| "Cannot identify shortcut portal session".to_string())?
        .as_str()
        .ok_or("Shortcut portal session has an invalid handle")?
        .to_owned();
    // One ordered stream is essential: merging separate press/release streams
    // can reorder a fast tap when both streams already have buffered events.
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(PORTAL_BUS_NAME)
        .map_err(|e| portal_error("watch shortcut sender", e))?
        .path("/org/freedesktop/portal/desktop")
        .map_err(|e| portal_error("watch shortcut path", e))?
        .interface("org.freedesktop.portal.GlobalShortcuts")
        .map_err(|e| portal_error("watch shortcut interface", e))?
        .build();
    let signals = MessageStream::for_match_rule(rule, connection, Some(64))
        .await
        .map_err(|e| portal_error("subscribe to ordered shortcut events", e))?
        .boxed();
    let bus = DBusProxy::new(connection)
        .await
        .map_err(|e| portal_error("watch portal service", e))?;
    let bus_name = PORTAL_BUS_NAME
        .try_into()
        .map_err(|_| "Invalid portal service name")?;
    let owner = bus
        .get_name_owner(bus_name)
        .await
        .map_err(|e| portal_error("identify portal service", e))?
        .to_string();
    let shortcuts: Vec<_> = bindings
        .values()
        .map(|binding| {
            let trigger = to_portal_trigger(&binding.current_binding)?;
            Ok(NewShortcut::new(&binding.id, &binding.name)
                .preferred_trigger(Some(trigger.as_str())))
        })
        .collect::<Result<_, String>>()?;
    {
        let mut closed = session
            .receive_closed()
            .await
            .map_err(|e| portal_error("watch session", e))?;
        let mut owners = bus
            .receive_name_owner_changed_with_args(&[(0, PORTAL_BUS_NAME)])
            .await
            .map_err(|e| portal_error("watch portal owner", e))?;
        let request = tokio::select! {
            result = proxy.bind_shortcuts(&session, &shortcuts, None, Default::default()) => result.map_err(|e| portal_error("bind shortcuts", e))?,
            _ = closed.next() => return Err("The shortcut portal session closed during registration.".into()),
            _ = owners.next() => return Err("The desktop portal restarted during shortcut registration. Retry Portal.".into()),
        };
        let response = request
            .response()
            .map_err(|e| portal_error("confirm shortcuts", e))?;
        validate_approved_bindings(
            bindings,
            response
                .shortcuts()
                .iter()
                .map(|s| (s.id(), s.trigger_description())),
        )?;
        for shortcut in response.shortcuts() {
            info!(
                "Shortcut portal assigned action {}: {}",
                shortcut.id(),
                safe_error(shortcut.trigger_description())
            );
        }
    }
    Ok(Prepared {
        proxy,
        session,
        key: SessionKey { generation, path },
        owner,
        signals,
    })
}

#[allow(clippy::too_many_arguments)]
async fn session_worker(
    app: AppHandle,
    bindings: Bindings,
    generation: u64,
    dispatch: Arc<Mutex<DispatchState>>,
    mut shutdown: watch::Receiver<bool>,
    mut cancelled: watch::Receiver<bool>,
    mut local_stop: watch::Receiver<bool>,
    ready: oneshot::Sender<Result<Connection, String>>,
    budget: Duration,
) {
    let mut ready = Some(ready);
    let mut connection = None;
    let mut installed_key = None;
    let app_id = app.config().identifier.clone();
    let body = async {
        let connect = Connection::session()
            .await
            .map_err(|e| portal_error("connect to session bus", e))?;
        connection = Some(connect.clone());
        let prepared = prepare(&connect, &app_id, &bindings, generation).await?;
        Ok::<_, String>((connect, prepared))
    };
    let prepared = tokio::select! {
        result = tokio::time::timeout(budget, body) => match result {
            Ok(result) => result,
            Err(_) => Err("Shortcut permission request timed out. Select Portal again and respond to the desktop dialog.".into()),
        },
        _ = wait_cancelled(&mut shutdown) => Err("Shortcut portal initialization cancelled.".into()),
        _ = wait_cancelled(&mut local_stop) => Err("Shortcut portal initialization cancelled.".into()),
        _ = wait_cancelled(&mut cancelled) => Err("Shortcut request was cancelled.".into()),
    };
    let result = match prepared {
        Ok((connect, prepared)) => {
            installed_key = Some(prepared.key.clone());
            run_session(
                &app,
                connect,
                prepared,
                bindings,
                &dispatch,
                &mut ready,
                &mut shutdown,
                &mut local_stop,
                &mut cancelled,
            )
            .await
        }
        Err(error) => Err(error),
    };
    if let Some(reply) = ready.take() {
        let _ = reply.send(Err(result.clone().err().unwrap_or_else(|| {
            "Shortcut portal stopped before registration completed.".into()
        })));
    }
    let was_active = installed_key.as_ref().is_some_and(|key| {
        dispatch
            .lock()
            .map(|s| s.active.as_ref() == Some(key))
            .unwrap_or(false)
    });
    let held = installed_key
        .as_ref()
        .and_then(|key| dispatch.lock().ok().map(|mut s| s.clear(Some(key))))
        .unwrap_or(false);
    cancel_owned_operation(&app, was_active, held);
    if let Err(ref error) = result {
        if was_active && !*shutdown.borrow() && !*local_stop.borrow() {
            report_failure(&app, error);
        }
    }
    if let Some(connection) = connection {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, connection.close()).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    app: &AppHandle,
    connection: Connection,
    mut prepared: Prepared,
    bindings: Bindings,
    dispatch: &Arc<Mutex<DispatchState>>,
    ready: &mut Option<oneshot::Sender<Result<Connection, String>>>,
    shutdown: &mut watch::Receiver<bool>,
    local_stop: &mut watch::Receiver<bool>,
    cancelled: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let mut closed = prepared
        .session
        .receive_closed()
        .await
        .map_err(|e| portal_error("watch session", e))?;
    let bus = DBusProxy::new(&connection)
        .await
        .map_err(|e| portal_error("watch portal service", e))?;
    let mut owners = bus
        .receive_name_owner_changed_with_args(&[(0, PORTAL_BUS_NAME)])
        .await
        .map_err(|e| portal_error("watch portal owner", e))?;
    let bus_name = PORTAL_BUS_NAME
        .try_into()
        .map_err(|_| "Invalid portal service name")?;
    let current_owner = bus
        .get_name_owner(bus_name)
        .await
        .map_err(|e| portal_error("check portal service", e))?;
    if current_owner.as_str() != prepared.owner {
        return Err("The desktop portal restarted during registration. Retry Portal.".into());
    }
    if *shutdown.borrow() || *local_stop.borrow() || *cancelled.borrow() {
        return Err("Shortcut registration was cancelled before it became active.".into());
    }
    dispatch
        .lock()
        .map_err(|_| "Shortcut dispatch lock poisoned")?
        .install(prepared.key.clone(), bindings.clone());
    for id in bindings.keys() {
        info!("Shortcut portal registered action: {id}");
    }
    let reply = ready.take().ok_or("Shortcut readiness response missing")?;
    if reply.send(Ok(connection.clone())).is_err() {
        return Err("Shortcut registration caller was cancelled.".into());
    }
    info!("Shortcut portal ready");
    let result = loop {
        tokio::select! {
            biased;
            _ = wait_cancelled(shutdown) => break Ok(()),
            _ = wait_cancelled(local_stop) => break Ok(()),
            _ = wait_cancelled(cancelled) => break Err("Shortcut request was cancelled.".into()),
            _ = closed.next() => break Err("The desktop closed Handy's shortcut session. Select Portal again to reconnect.".into()),
            _ = owners.next() => break Err("The desktop portal restarted. Select Portal again to reconnect shortcuts.".into()),
            message = prepared.signals.next() => match message {
                Some(Ok(message)) => {
                    if let Err(error) = dispatch_signal(app, dispatch, &prepared.key, &bindings, message) {
                        break Err(error);
                    }
                }
                Some(Err(error)) => break Err(portal_error("receive shortcut events", error)),
                None => break Err("Shortcut event stream ended. Select Portal again to reconnect.".into()),
            },
        }
    };
    drop(closed);
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, prepared.session.close()).await;
    drop(prepared.proxy);
    result
}

fn validate_approved_bindings<'a>(
    bindings: &Bindings,
    approved: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<(), String> {
    let approved: BTreeMap<_, _> = approved.into_iter().collect();
    for id in bindings.keys() {
        match approved.get(id.as_str()) {
            Some(trigger) if !trigger.trim().is_empty() => {}
            Some(_) => return Err(format!("No key is assigned to Handy action {id}. Assign its shortcut in the desktop portal dialog or GNOME Keyboard Settings, then retry Portal.")),
            None => return Err("The desktop did not approve all requested Handy shortcuts. Review its shortcut permissions and retry.".into()),
        }
    }
    Ok(())
}

fn dispatch_signal(
    app: &AppHandle,
    dispatch: &Arc<Mutex<DispatchState>>,
    key: &SessionKey,
    bindings: &Bindings,
    message: Message,
) -> Result<(), String> {
    match message.header().member().map(|member| member.as_str()) {
        Some("Activated") => {
            let event: Activated = message
                .body()
                .deserialize()
                .map_err(|e| portal_error("decode activation", e))?;
            dispatch_event(
                app,
                dispatch,
                key,
                event.session_handle().as_str(),
                event.shortcut_id(),
                true,
            );
        }
        Some("Deactivated") => {
            let event: Deactivated = message
                .body()
                .deserialize()
                .map_err(|e| portal_error("decode deactivation", e))?;
            dispatch_event(
                app,
                dispatch,
                key,
                event.session_handle().as_str(),
                event.shortcut_id(),
                false,
            );
        }
        Some("ShortcutsChanged") => {
            let event: ShortcutsChanged = message
                .body()
                .deserialize()
                .map_err(|e| portal_error("decode shortcut update", e))?;
            if event.session_handle().as_str() == key.path {
                validate_approved_bindings(
                    bindings,
                    event
                        .shortcuts()
                        .iter()
                        .map(|shortcut| (shortcut.id(), shortcut.trigger_description())),
                )?;
                info!("Shortcut portal mapping updated");
            }
        }
        _ => {}
    }
    Ok(())
}

fn dispatch_event(
    app: &AppHandle,
    dispatch: &Arc<Mutex<DispatchState>>,
    key: &SessionKey,
    path: &str,
    id: &str,
    pressed: bool,
) {
    let binding = dispatch
        .lock()
        .ok()
        .and_then(|mut state| state.event(key, path, id, pressed));
    if let Some(binding) = binding {
        info!(
            "Portal shortcut {}: {}",
            if pressed { "Activated" } else { "Deactivated" },
            binding.id
        );
        handle_shortcut_event(app, &binding.id, &binding.current_binding, pressed);
    }
}

fn cancel_owned_operation(app: &AppHandle, owned: bool, held: bool) {
    // A retiring session must never cancel a newer generation's dictation.
    if !owned {
        return;
    }
    let recording = app
        .try_state::<Arc<AudioRecordingManager>>()
        .is_some_and(|manager| manager.is_recording());
    if (held || recording)
        && app.try_state::<Arc<AudioRecordingManager>>().is_some()
        && app.try_state::<Arc<TranscriptionManager>>().is_some()
    {
        info!("Cancelling dictation because its shortcut session ended");
        crate::utils::cancel_current_operation(app);
    }
}

fn report_failure(app: &AppHandle, detail: &str) {
    let detail = safe_error(detail);
    error!("Shortcut portal failure: {detail}");
    notices::raise(
        app,
        Notice::new(
            NoticeCode::ShortcutRegistrationFailed,
            NoticeSource::Shortcuts,
        )
        .with_detail(Some(detail)),
    );
}

fn portal_error(context: &str, error: impl std::fmt::Display) -> String {
    format!(
        "Shortcut portal could not {context}: {}",
        safe_error(&error.to_string())
    )
}

fn safe_error(raw: &str) -> String {
    raw.split_whitespace()
        .map(|word| {
            if word.contains("/org/freedesktop/portal/") {
                "[portal handle]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(500)
        .collect()
}

/// Convert a configured key chord into the portal's preferred-trigger syntax.
fn to_portal_trigger(raw: &str) -> Result<String, String> {
    let mut modifiers = BTreeSet::new();
    let mut key = None;
    for part in raw.split('+') {
        let part = part.trim();
        if part.is_empty() {
            return Err("Shortcut contains an empty key.".into());
        }
        let lower = part.to_ascii_lowercase();
        let modifier = match lower.as_str() {
            "ctrl" | "control" => Some("CTRL"),
            "alt" | "option" => Some("ALT"),
            "shift" => Some("SHIFT"),
            "super" | "meta" | "cmd" | "command" | "win" | "windows" => Some("LOGO"),
            "numlock" | "num" => Some("NUM"),
            _ => None,
        };
        if let Some(modifier) = modifier {
            modifiers.insert(modifier);
            continue;
        }
        if key.is_some() {
            return Err("A shortcut must have exactly one main key.".into());
        }
        key = Some(canonical_key(&lower, part)?);
    }
    let key = key.ok_or("Portal shortcuts need a main key in addition to modifiers.")?;
    let mut parts: Vec<_> = ["CTRL", "ALT", "SHIFT", "LOGO", "NUM"]
        .into_iter()
        .filter(|part| modifiers.contains(part))
        .map(str::to_owned)
        .collect();
    parts.push(key);
    Ok(parts.join("+"))
}

fn canonical_key(lower: &str, original: &str) -> Result<String, String> {
    let name = match lower {
        "space" => "space",
        "esc" | "escape" => "Escape",
        "enter" | "return" => "Return",
        "backspace" => "BackSpace",
        "tab" => "Tab",
        "delete" | "del" => "Delete",
        "insert" | "ins" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" => "Page_Up",
        "pagedown" | "page_down" => "Page_Down",
        "left" | "arrowleft" => "Left",
        "right" | "arrowright" => "Right",
        "up" | "arrowup" => "Up",
        "down" | "arrowdown" => "Down",
        "grave" | "backquote" | "`" => "grave",
        "plus" => "plus",
        "minus" | "-" => "minus",
        "equal" | "equals" | "=" => "equal",
        "bracketleft" | "[" => "bracketleft",
        "bracketright" | "]" => "bracketright",
        "semicolon" | ";" => "semicolon",
        "apostrophe" | "quote" | "'" => "apostrophe",
        "backslash" | "\\" => "backslash",
        "comma" | "," => "comma",
        "period" | "dot" | "." => "period",
        "slash" | "/" => "slash",
        _ => {
            if lower.len() == 1 && lower.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                return Ok(lower.to_owned());
            }
            if let Some(number) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                if (1..=35).contains(&number) {
                    return Ok(format!("F{number}"));
                }
            }
            if original.starts_with("XF86") && original.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Ok(original.to_owned());
            }
            return Err("This key is not supported by the portal shortcut converter. Use a letter, number, F-key, or standard navigation key.".into());
        }
    };
    Ok(name.to_owned())
}

pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    to_portal_trigger(raw).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(id: &str) -> ShortcutBinding {
        ShortcutBinding {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            default_binding: "ctrl+alt+space".into(),
            current_binding: "ctrl+alt+space".into(),
        }
    }

    #[test]
    fn converts_aliases_and_named_keys_without_empty_or_modifier_only_chords() {
        assert_eq!(
            to_portal_trigger("control+option+space").unwrap(),
            "CTRL+ALT+space"
        );
        assert_eq!(
            to_portal_trigger("super+shift+f12").unwrap(),
            "SHIFT+LOGO+F12"
        );
        assert_eq!(to_portal_trigger("ctrl+enter").unwrap(), "CTRL+Return");
        assert_eq!(to_portal_trigger("ctrl+grave").unwrap(), "CTRL+grave");
        assert_eq!(to_portal_trigger("ctrl+plus").unwrap(), "CTRL+plus");
        assert_eq!(to_portal_trigger("super+A").unwrap(), "LOGO+a");
        assert_eq!(to_portal_trigger("alt+7").unwrap(), "ALT+7");
        assert_eq!(to_portal_trigger("ctrl+esc").unwrap(), "CTRL+Escape");
        for invalid in [
            "",
            "ctrl+alt",
            "ctrl++space",
            "ctrl+a+b",
            "ctrl+ space\nsecret",
        ] {
            assert!(validate_shortcut(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn dispatch_filters_other_sessions_unknown_ids_and_repeat_events() {
        let key = SessionKey {
            generation: 4,
            path: "/session/current".into(),
        };
        let old = SessionKey {
            generation: 3,
            path: "/session/current".into(),
        };
        let mut state = DispatchState::default();
        state.install(
            key.clone(),
            [("transcribe".into(), binding("transcribe"))].into(),
        );
        assert!(state.event(&old, &key.path, "transcribe", true).is_none());
        assert!(state
            .event(&key, "/session/other", "transcribe", true)
            .is_none());
        assert!(state.event(&key, &key.path, "unknown", true).is_none());
        assert!(state.event(&key, &key.path, "transcribe", false).is_none());
        assert!(state.event(&key, &key.path, "transcribe", true).is_some());
        assert!(state.event(&key, &key.path, "transcribe", true).is_none());
        assert!(state.event(&key, &key.path, "transcribe", false).is_some());
        assert!(state.event(&key, &key.path, "transcribe", false).is_none());
    }

    #[test]
    fn stale_session_cannot_cancel_current_hold_and_close_clears_pressed_state() {
        let key = SessionKey {
            generation: 2,
            path: "/session/current".into(),
        };
        let stale = SessionKey {
            generation: 1,
            path: "/session/stale".into(),
        };
        let mut state = DispatchState::default();
        state.install(
            key.clone(),
            [("transcribe".into(), binding("transcribe"))].into(),
        );
        assert!(state.event(&key, &key.path, "transcribe", true).is_some());
        assert!(!state.clear(Some(&stale)));
        assert!(state.clear(Some(&key)));
        assert!(!state.clear(Some(&key)));
        assert!(state.event(&key, &key.path, "transcribe", false).is_none());
    }

    #[test]
    fn queued_quick_taps_and_represses_preserve_event_order() {
        let key = SessionKey {
            generation: 1,
            path: "/session/current".into(),
        };
        let mut state = DispatchState::default();
        state.install(
            key.clone(),
            [("transcribe".into(), binding("transcribe"))].into(),
        );
        let queued = [true, false, true, false];
        let delivered: Vec<_> = queued
            .into_iter()
            .filter(|pressed| {
                state
                    .event(&key, &key.path, "transcribe", *pressed)
                    .is_some()
            })
            .collect();
        assert_eq!(delivered, queued);
        assert!(state.held.is_empty());
    }

    #[test]
    fn partial_or_empty_portal_approval_is_not_success() {
        let requested = [
            ("transcribe".into(), binding("transcribe")),
            (
                "transcribe_with_post_process".into(),
                binding("transcribe_with_post_process"),
            ),
        ]
        .into();
        assert!(validate_approved_bindings(&requested, []).is_err());
        assert!(validate_approved_bindings(
            &requested,
            [
                ("transcribe", ""),
                ("transcribe_with_post_process", "Ctrl+Alt+Shift+Space")
            ]
        )
        .is_err());
        assert!(
            validate_approved_bindings(&requested, [("transcribe", "Ctrl+Alt+Space")]).is_err()
        );
        assert!(validate_approved_bindings(
            &requested,
            [
                ("transcribe", "Ctrl+Alt+Space"),
                ("transcribe_with_post_process", "Ctrl+Alt+Shift+Space")
            ]
        )
        .is_ok());
    }

    #[test]
    fn diagnostics_do_not_expose_portal_handles() {
        let detail = safe_error(
            "Failed for /org/freedesktop/portal/desktop/session/1_2/private_token\nretry",
        );
        assert!(!detail.contains("private_token"));
        assert!(!detail.contains('\n'));
    }
}
