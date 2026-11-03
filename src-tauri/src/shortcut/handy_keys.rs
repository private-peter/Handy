//! Handy-keys based keyboard shortcut implementation
//!
//! This module provides an alternative to Tauri's global-shortcut plugin
//! using the handy-keys library for more control over keyboard events.
//!
//! ## Architecture
//!
//! The implementation uses a dedicated manager thread that owns the `HotkeyManager`:
//!
//! ```text
//! ┌─────────────────┐     commands      ┌──────────────────────┐
//! │   Main Thread   │ ───────────────▶ │   Manager Thread     │
//! │                 │   (via channel)   │                      │
//! │ - register()    │                   │ - owns HotkeyManager │
//! │ - unregister()  │                   │ - polls for events   │
//! └─────────────────┘                   │ - dispatches actions │
//!                                       └──────────────────────┘
//! ```
//!
//! This design ensures thread-safety since `HotkeyManager` is only accessed
//! from a single thread. Commands (register/unregister) are sent via an mpsc
//! channel and responses are synchronously awaited.
//!
//! ## Recording Mode
//!
//! For UI key capture, a separate `KeyboardListener` is created on-demand and
//! polled from a dedicated recording thread. Events are emitted to the frontend
//! via Tauri's event system.

use handy_keys::{Hotkey, HotkeyId, HotkeyManager, HotkeyState, KeyboardListener};
use log::{debug, error, info};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tauri::{AppHandle, Emitter, Manager};

use crate::settings::{self, get_settings, ShortcutBinding};

use super::handler::handle_shortcut_event;

/// Commands that can be sent to the hotkey manager thread
enum ManagerCommand {
    Register {
        binding_id: String,
        hotkey_string: String,
        response: Sender<Result<(), String>>,
    },
    Unregister {
        binding_id: String,
        response: Sender<Result<(), String>>,
    },
    Shutdown,
}

/// A live manager owns the native device handles on its thread.
struct ManagerActor {
    command_sender: Sender<ManagerCommand>,
    thread_handle: JoinHandle<()>,
}

/// Permanently managed state. Backend switching takes/stops the actor instead
/// of removing Tauri state, which would invalidate outstanding State references.
pub struct HandyKeysState {
    /// Serializes startup, capture startup, and shutdown.
    lifecycle: Mutex<()>,
    manager: Mutex<Option<ManagerActor>>,
    /// Recording listener for UI key capture (only active during recording)
    recording_listener: Mutex<Option<KeyboardListener>>,
    /// Flag indicating if we're in recording mode
    is_recording: AtomicBool,
    /// The binding ID being recorded (if any)
    recording_binding_id: Mutex<Option<String>>,
    /// Flag to stop recording loop
    recording_running: Arc<AtomicBool>,
    recording_thread: Mutex<Option<JoinHandle<()>>>,
}

/// Key event sent to frontend during recording mode
#[derive(Debug, Clone, Serialize, Type)]
pub struct FrontendKeyEvent {
    /// Currently pressed modifier keys
    pub modifiers: Vec<String>,
    /// The key that was pressed (if any)
    pub key: Option<String>,
    /// Whether this is a key down event
    pub is_key_down: bool,
    /// The full hotkey string (e.g., "option+space")
    pub hotkey_string: String,
}

impl HandyKeysState {
    /// Inactive state can safely be managed before any input devices are opened.
    fn new() -> Self {
        Self {
            lifecycle: Mutex::new(()),
            manager: Mutex::new(None),
            recording_listener: Mutex::new(None),
            is_recording: AtomicBool::new(false),
            recording_binding_id: Mutex::new(None),
            recording_running: Arc::new(AtomicBool::new(false)),
            recording_thread: Mutex::new(None),
        }
    }

    /// Whether the native manager is alive. State existence alone is not enough
    /// after switching away from this backend.
    pub fn is_running(&self) -> bool {
        self.manager
            .lock()
            .map(|manager| {
                manager
                    .as_ref()
                    .is_some_and(|actor| !actor.thread_handle.is_finished())
            })
            .unwrap_or(false)
    }

    /// Called under lifecycle so initialization cannot race cleanup or capture.
    fn start_manager(&self, app: AppHandle) -> Result<bool, String> {
        let mut slot = self
            .manager
            .lock()
            .map_err(|_| "Failed to lock native manager")?;
        if let Some(actor) = slot.as_ref() {
            if !actor.thread_handle.is_finished() {
                return Ok(false);
            }
        }
        if let Some(actor) = slot.take() {
            let _ = actor.thread_handle.join();
        }
        // Permission failures reach the caller; no managed state is replaced.
        let manager = HotkeyManager::new_with_blocking()
            .map_err(|e| format!("Failed to create HotkeyManager: {e}"))?;
        let (command_sender, command_receiver) = mpsc::channel();
        let thread_handle = thread::spawn(move || {
            Self::manager_thread(command_receiver, app, manager);
        });
        *slot = Some(ManagerActor {
            command_sender,
            thread_handle,
        });
        Ok(true)
    }

    /// Stop every native reader before another backend is selected.
    fn shutdown(&self) -> Result<(), String> {
        let _lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        self.stop_recording_inner();
        let mut slot = self.manager.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(actor) = slot.take() {
            let _ = actor.command_sender.send(ManagerCommand::Shutdown);
            actor
                .thread_handle
                .join()
                .map_err(|_| "Native shortcut manager panicked during shutdown")?;
        }
        Ok(())
    }

    /// The main manager thread - owns the HotkeyManager and processes commands
    fn manager_thread(cmd_rx: Receiver<ManagerCommand>, app: AppHandle, manager: HotkeyManager) {
        info!("handy-keys manager thread started");

        // Maps binding IDs to HotkeyIds and hotkey strings
        let mut binding_to_hotkey: HashMap<String, HotkeyId> = HashMap::new();
        let mut hotkey_to_binding: HashMap<HotkeyId, (String, String)> = HashMap::new(); // (binding_id, hotkey_string)

        loop {
            // Check for hotkey events (non-blocking)
            while let Some(event) = manager.try_recv() {
                if let Some((binding_id, hotkey_string)) = hotkey_to_binding.get(&event.id) {
                    debug!(
                        "handy-keys event: binding={}, hotkey={}, state={:?}",
                        binding_id, hotkey_string, event.state
                    );
                    let is_pressed = event.state == HotkeyState::Pressed;
                    handle_shortcut_event(&app, binding_id, hotkey_string, is_pressed);
                }
            }

            // Check for commands (non-blocking with timeout)
            match cmd_rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(cmd) => match cmd {
                    ManagerCommand::Register {
                        binding_id,
                        hotkey_string,
                        response,
                    } => {
                        let result = Self::do_register(
                            &manager,
                            &mut binding_to_hotkey,
                            &mut hotkey_to_binding,
                            &binding_id,
                            &hotkey_string,
                        );
                        let _ = response.send(result);
                    }
                    ManagerCommand::Unregister {
                        binding_id,
                        response,
                    } => {
                        let result = Self::do_unregister(
                            &manager,
                            &mut binding_to_hotkey,
                            &mut hotkey_to_binding,
                            &binding_id,
                        );
                        let _ = response.send(result);
                    }
                    ManagerCommand::Shutdown => {
                        info!("handy-keys manager thread shutting down");
                        break;
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No command, continue
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    info!("Command channel disconnected, shutting down");
                    break;
                }
            }
        }

        info!("handy-keys manager thread stopped");
    }

    /// Register a hotkey
    fn do_register(
        manager: &HotkeyManager,
        binding_to_hotkey: &mut HashMap<String, HotkeyId>,
        hotkey_to_binding: &mut HashMap<HotkeyId, (String, String)>,
        binding_id: &str,
        hotkey_string: &str,
    ) -> Result<(), String> {
        let hotkey: Hotkey = hotkey_string
            .parse()
            .map_err(|e| format!("Failed to parse hotkey '{}': {}", hotkey_string, e))?;

        let id = manager
            .register(hotkey)
            .map_err(|e| format!("Failed to register hotkey: {}", e))?;

        binding_to_hotkey.insert(binding_id.to_string(), id);
        hotkey_to_binding.insert(id, (binding_id.to_string(), hotkey_string.to_string()));

        debug!(
            "Registered handy-keys shortcut: {} -> {:?}",
            binding_id, hotkey
        );
        Ok(())
    }

    /// Unregister a hotkey
    fn do_unregister(
        manager: &HotkeyManager,
        binding_to_hotkey: &mut HashMap<String, HotkeyId>,
        hotkey_to_binding: &mut HashMap<HotkeyId, (String, String)>,
        binding_id: &str,
    ) -> Result<(), String> {
        if let Some(id) = binding_to_hotkey.remove(binding_id) {
            manager
                .unregister(id)
                .map_err(|e| format!("Failed to unregister hotkey: {}", e))?;
            hotkey_to_binding.remove(&id);
            debug!("Unregistered handy-keys shortcut: {}", binding_id);
        }
        Ok(())
    }

    /// Register a shortcut binding. Holding the actor lock until its reply
    /// prevents shutdown/restart from overtaking an in-flight registration.
    pub fn register(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let slot = self
            .manager
            .lock()
            .map_err(|_| "Failed to lock native manager")?;
        let actor = slot.as_ref().ok_or("Handy Keys is not running")?;
        let (tx, rx) = mpsc::channel();
        actor
            .command_sender
            .send(ManagerCommand::Register {
                binding_id: binding.id.clone(),
                hotkey_string: binding.current_binding.clone(),
                response: tx,
            })
            .map_err(|_| "Failed to send register command")?;
        rx.recv()
            .map_err(|_| "Failed to receive register response")?
    }

    /// Unregister a shortcut binding.
    pub fn unregister(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let slot = self
            .manager
            .lock()
            .map_err(|_| "Failed to lock native manager")?;
        let actor = slot.as_ref().ok_or("Handy Keys is not running")?;
        let (tx, rx) = mpsc::channel();
        actor
            .command_sender
            .send(ManagerCommand::Unregister {
                binding_id: binding.id.clone(),
                response: tx,
            })
            .map_err(|_| "Failed to send unregister command")?;
        rx.recv()
            .map_err(|_| "Failed to receive unregister response")?
    }

    /// Start recording mode for a specific binding
    pub fn start_recording(&self, app: &AppHandle, binding_id: String) -> Result<(), String> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| "Failed to lock native lifecycle")?;
        if !self.is_running() {
            return Err("Handy Keys is not running".into());
        }
        if self.is_recording.load(Ordering::SeqCst) {
            return Err("Already recording".into());
        }

        // Create a new keyboard listener for recording
        let listener = KeyboardListener::new()
            .map_err(|e| format!("Failed to create keyboard listener: {}", e))?;

        {
            let mut recording = self
                .recording_listener
                .lock()
                .map_err(|_| "Failed to lock recording_listener")?;
            *recording = Some(listener);
        }
        {
            let mut binding = self
                .recording_binding_id
                .lock()
                .map_err(|_| "Failed to lock recording_binding_id")?;
            *binding = Some(binding_id);
        }

        self.is_recording.store(true, Ordering::SeqCst);
        self.recording_running.store(true, Ordering::SeqCst);

        // Start a thread to emit key events to the frontend
        let app_clone = app.clone();
        let recording_running = Arc::clone(&self.recording_running);
        let handle = thread::spawn(move || {
            Self::recording_loop(app_clone, recording_running);
        });
        *self
            .recording_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle);

        debug!("Started handy-keys recording mode");
        Ok(())
    }

    /// Recording loop - emits key events to frontend during recording
    fn recording_loop(app: AppHandle, running: Arc<AtomicBool>) {
        while running.load(Ordering::SeqCst) {
            let event = {
                let state = match app.try_state::<HandyKeysState>() {
                    Some(s) => s,
                    None => break,
                };
                let listener = state.recording_listener.lock().ok();
                listener.as_ref().and_then(|l| l.as_ref()?.try_recv())
            };

            if let Some(key_event) = event {
                // Convert to frontend-friendly format
                let frontend_event = FrontendKeyEvent {
                    modifiers: modifiers_to_strings(key_event.modifiers),
                    key: key_event.key.map(|k| k.to_string().to_lowercase()),
                    is_key_down: key_event.is_key_down,
                    hotkey_string: key_event
                        .as_hotkey()
                        .map(|h| h.to_handy_string())
                        .unwrap_or_default(),
                };

                // Emit to frontend
                if let Err(e) = app.emit("handy-keys-event", &frontend_event) {
                    error!("Failed to emit key event: {}", e);
                }
            } else {
                thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        debug!("Recording loop ended");
    }

    /// Stop recording mode. Cleanup joins the old reader so a new capture
    /// cannot accidentally revive its shared running flag.
    pub fn stop_recording(&self) -> Result<(), String> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| "Failed to lock native lifecycle")?;
        self.stop_recording_inner();
        Ok(())
    }

    fn stop_recording_inner(&self) {
        self.is_recording.store(false, Ordering::SeqCst);
        self.recording_running.store(false, Ordering::SeqCst);
        *self
            .recording_listener
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .recording_binding_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(handle) = self
            .recording_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }
        debug!("Stopped handy-keys recording mode");
    }
}

impl Drop for HandyKeysState {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Convert handy-keys Modifiers to a list of strings
fn modifiers_to_strings(modifiers: handy_keys::Modifiers) -> Vec<String> {
    let mut result = Vec::new();

    if modifiers.contains(handy_keys::Modifiers::CTRL) {
        result.push("ctrl".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::OPT) {
        #[cfg(target_os = "macos")]
        result.push("option".to_string());
        #[cfg(not(target_os = "macos"))]
        result.push("alt".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::SHIFT) {
        result.push("shift".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::CMD) {
        #[cfg(target_os = "macos")]
        result.push("command".to_string());
        #[cfg(not(target_os = "macos"))]
        result.push("super".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::FN) {
        result.push("fn".to_string());
    }

    result
}

/// Validate a shortcut string for the HandyKeys implementation.
/// HandyKeys is more permissive: allows modifier-only combos and the fn key.
pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err("Shortcut cannot be empty".into());
    }
    // HandyKeys accepts modifier-only, key-only, and modifier+key combos
    // Just verify the string is parseable
    raw.parse::<Hotkey>()
        .map(|_| ())
        .map_err(|e| format!("Invalid shortcut for HandyKeys: {}", e))
}

/// Initialize or restart the native backend without ever removing managed state.
pub fn init_shortcuts(app: &AppHandle) -> Result<(), String> {
    if app.try_state::<HandyKeysState>().is_none() {
        // Manage an inactive value first: losing a concurrent manage race cannot
        // create a second keyboard grab or leak a native reader.
        app.manage(HandyKeysState::new());
    }
    let state = app.state::<HandyKeysState>();
    let _lifecycle = state
        .lifecycle
        .lock()
        .map_err(|_| "Failed to lock native lifecycle")?;
    if !state.start_manager(app.clone())? {
        return Ok(());
    }
    let default_bindings = settings::get_default_settings().bindings;
    let user_settings = settings::load_or_create_app_settings(app);
    for (id, default_binding) in default_bindings {
        if id == "cancel"
            || (id == "transcribe_with_post_process" && !user_settings.post_process_enabled)
        {
            continue;
        }
        let binding = user_settings
            .bindings
            .get(&id)
            .cloned()
            .unwrap_or(default_binding);
        if let Err(error) = state.register(&binding) {
            // The initialization caller can report/roll back a failed native
            // startup. Do not leave device handles alive on partial failure.
            let mut slot = state.manager.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(actor) = slot.take() {
                let _ = actor.command_sender.send(ManagerCommand::Shutdown);
                let _ = actor.thread_handle.join();
            }
            return Err(format!("Failed to register native shortcut {id}: {error}"));
        }
    }
    info!("handy-keys shortcuts initialized");
    Ok(())
}

/// Close native device handles in-place. State references remain valid across
/// backend changes, including capture callbacks already in flight.
pub fn cleanup_shortcuts(app: &AppHandle) {
    if let Some(state) = app.try_state::<HandyKeysState>() {
        if let Err(error) = state.shutdown() {
            error!("Failed to stop native shortcut manager: {error}");
        }
    }
}

/// Register a shortcut
pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.register(&binding)
}

/// Unregister a shortcut
pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.unregister(&binding)
}

/// Start key recording mode
#[tauri::command]
#[specta::specta]
pub async fn start_handy_keys_recording(app: AppHandle, binding_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || start_handy_keys_recording_inner(app, binding_id))
        .await
        .map_err(|error| format!("Shortcut capture task failed: {error}"))?
}

fn start_handy_keys_recording_inner(app: AppHandle, binding_id: String) -> Result<(), String> {
    let _configuration = super::lock_configuration();
    let settings = get_settings(&app);
    if settings.keyboard_implementation != settings::KeyboardImplementation::HandyKeys {
        return Err("handy-keys is not the active keyboard implementation".into());
    }

    // While Secure Input is active the tap receives no KeyDown/KeyUp, so the
    // recorder would silently capture just the modifier and overwrite the
    // binding with it (issue #1578). Refuse instead; the frontend maps this
    // marker to a localized explanation, and the noted impact makes the
    // warning banner appear with the full story.
    if crate::secure_input::is_enabled_now() {
        crate::secure_input::note_recorder_blocked(&app);
        return Err("secure-input-active".into());
    }

    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;

    // Suspend every registered shortcut so a combo that overlaps an existing
    // binding can't fire it (or have its keys swallowed) mid-capture.
    super::suspend_all_shortcuts(&app);

    let result = state.start_recording(&app, binding_id);
    if result.is_err() {
        super::resume_all_shortcuts(&app);
    }
    result
}

/// Stop key recording mode
#[tauri::command]
#[specta::specta]
pub async fn stop_handy_keys_recording(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || stop_handy_keys_recording_inner(app))
        .await
        .map_err(|error| format!("Shortcut capture task failed: {error}"))?
}

fn stop_handy_keys_recording_inner(app: AppHandle) -> Result<(), String> {
    let _configuration = super::lock_configuration();
    let settings = get_settings(&app);
    if settings.keyboard_implementation != settings::KeyboardImplementation::HandyKeys {
        return Err("handy-keys is not the active keyboard implementation".into());
    }

    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;

    // Restore shortcuts from settings regardless of how recording ended.
    // A commit has already registered the new binding via change_binding;
    // re-registering it here fails cleanly and is ignored.
    let result = state.stop_recording();
    super::resume_all_shortcuts(&app);
    result
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn shutdown_joins_actor_and_clears_native_state() {
        let state = HandyKeysState::new();
        let finished = Arc::new(AtomicBool::new(false));
        let thread_finished = finished.clone();
        let (command_sender, command_receiver) = mpsc::channel();
        let thread_handle = thread::spawn(move || {
            while let Ok(command) = command_receiver.recv() {
                if matches!(command, ManagerCommand::Shutdown) {
                    break;
                }
            }
            thread_finished.store(true, Ordering::SeqCst);
        });
        *state.manager.lock().unwrap() = Some(ManagerActor {
            command_sender,
            thread_handle,
        });
        assert!(state.is_running());
        state.shutdown().unwrap();
        assert!(finished.load(Ordering::SeqCst));
        assert!(!state.is_running());
        assert!(!state.recording_running.load(Ordering::SeqCst));
        // Multiple backend cleanup calls are harmless and open no devices.
        state.shutdown().unwrap();
    }

    #[test]
    fn inactive_managed_state_does_not_accept_commands_or_capture() {
        let state = HandyKeysState::new();
        let binding = ShortcutBinding {
            id: "transcribe".into(),
            name: "Transcribe".into(),
            description: String::new(),
            default_binding: "ctrl+space".into(),
            current_binding: "ctrl+space".into(),
        };
        assert!(state
            .register(&binding)
            .unwrap_err()
            .contains("not running"));
        assert!(state
            .unregister(&binding)
            .unwrap_err()
            .contains("not running"));
    }
}
