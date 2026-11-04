# Desktop Portal shortcuts on Wayland

The Linux Desktop Portal keyboard implementation asks the compositor to register named shortcuts. Activation and deactivation events support Handy's Hold, Toggle, and Auto shortcut behavior without reading raw input devices. It does not require input-group membership, udev input rules, `/dev/input`, or `/dev/uinput` access.

This backend is experimental and needs verification on the target desktop. Shortcut registration and text insertion use separate portal sessions. Paste keys use an explicitly approved, keyboard-only RemoteDesktop session.

## Application identity

GNOME requires a valid desktop application ID and a matching installed desktop entry. Handy uses `com.pais.handy` and `com.pais.handy.desktop`. The backend registers that host application ID on its own D-Bus connection before calling the shortcut portal. It does not install desktop entries or change system configuration automatically.

AppImage installations need a desktop entry whose Exec and Icon fields refer to the installed artifact and icon. Desktop-entry installation grants no raw input-device permission. Quit other Handy instances before switching builds, since builds share the application identity and profile. Back up the profile before testing an experimental build.

## Setup and verification

1. In Advanced, enable Experimental Features and choose **Desktop Portal** under Keyboard Implementation.
2. Review and approve the desktop shortcut dialog. **Set Up Shortcuts** retries registration after denial or missing application registration. The configured combination is a preference; the desktop may assign another combination. Use the desktop-approved trigger, which is also reported in technical diagnostics.
3. In General, set Shortcut Behavior to **Hold**. Prefer a shortcut without held modifiers: desktop release matching and the eventual paste chord can otherwise depend on physical modifier state. Assignments are managed by the desktop; Handy's preferred-shortcut field may differ from the approved trigger.
4. Focus a blank editor, hold the shortcut, and release it. Verify one activation and one deactivation. Repeat with long holds, quick press/release, and a focus change while held. Idle Escape must remain normal. Test registration after relaunch, denial, and recovery.
5. With a clipboard paste method selected, startup or selecting Portal automatically opens pasting permission once, before shortcuts become ready. After cancellation, timeout, revocation, or **Stop Pasting**, use **Set Up Pasting** beside the Portal selector to retry manually. Review the desktop's Remote Desktop consent dialog. Handy requests keyboard input only, with no pointer control, screen capture, clipboard sharing, or persistent grant. Setup is rejected during dictation or paste; recording starts are rejected while setup is open.
6. Dictate a non-sensitive sentence into a blank editor and verify automatic insertion after release. Test the selected paste method separately in each intended application, including terminals. Output never opens a permission dialog or falls back to X11 or raw devices. An unavailable session raises the Paste Failed notice. Existing clipboard-handling and restore-delay settings still apply.
7. **Stop Pasting** closes Handy's keyboard-control session and leaves shortcut registration intact. Quitting Handy or switching away from Portal also closes that session. Restart requests approval again because permission is not persisted. Automatic requests do not repeat after cancellation or Stop Pasting within the same Portal activation. GNOME's global stop-sharing control is broader and stops other sharing/control sessions too.

Initialization and reconnection errors appear in Handy's notice list. Failures retain the Portal setting instead of silently selecting another backend. A closed or revoked shortcut session cancels an in-progress hold. Changing or reconnecting shortcut registration also cancels an active recording, including one started through the CLI.

Only named registered shortcut events and technical portal status belong in diagnostics. Do not collect raw typing, transcripts, clipboard text, credentials, or permission restore tokens for debugging.

## Permissions and desktop indicators

GNOME uses its sharing/control indicator for input-only RemoteDesktop sessions as well as screen sharing. The indicator remains visible while Handy's keyboard-control session is active, although Handy requests no screen capture. **Stop Pasting** ends that session. Permissions granted to other input-injection clients are separate.

The portal authorizes keyboard injection into the desktop session while the approved session is active. This is a meaningful permission even without screen capture or raw keyboard-read access. Shortcut approval alone does not grant paste permission. No input-device group, udev rule, privileged helper, or host remapping service is installed by this backend.

## Shift+Insert in terminals and other applications

On Linux, Shift+Insert publishes the transcription to both CLIPBOARD and PRIMARY. VTE terminals use PRIMARY for this shortcut, while other applications may use CLIPBOARD. PRIMARY uses GTK's X11 selection bridge, including under GNOME Wayland, because a background native Wayland client cannot claim PRIMARY. Xwayland must be available. Keyboard injection still uses the approved portal session; this clipboard bridge grants no raw keyboard access.

As in the existing X11 implementation, the previous PRIMARY snapshot is text-only. Restoration waits at least 250 ms after key delivery and skips PRIMARY if its text has changed. Non-text PRIMARY contents are not preserved. Clipboard cleanup stays serialized with the paste and also runs after an injection failure. Select **Shift+Insert** as Paste Method to use this behavior.

## Returning to a standard build

Before reopening a build that does not support this backend, choose **Tauri Global Shortcut** or **None** in the portal build and quit it. Older builds do not understand the persisted `portal` value. Restore a profile backup only while Handy is closed.

## Build isolation

Follow the repository's build instructions. When using a container, mount only the source and required build caches. Do not mount the host home, SSH credentials, Docker socket, Wayland socket, session D-Bus, or input devices into the builder. Keep local build configuration, deployment paths, device configuration, and generated artifacts outside tracked source files. Test the resulting AppImage as a normal desktop application after building.

## Native paste implementation

`portal_input.rs` owns a private, registered application connection and a keyboard-only, nonpersistent RemoteDesktop session. It uses NotifyKeyboardKeysym for Ctrl+V, Ctrl+Shift+V, Shift+Insert, and optional submit keys. It does not connect to EIS. Only startup/backend activation setup or an explicit Set Up Pasting request may show consent. Output requests fail before setup or while busy; cancellation generations prevent stale requests. Session close or service loss invalidates readiness. Error and timeout cleanup releases possibly pressed keys and closes the connection before acknowledging completion.

Ashpd's `screencast` feature is needed to compile the RemoteDesktop response stream type. This dependency does not request screen capture: the implementation makes no ScreenCast API calls and rejects approval containing screen streams, clipboard sharing, or devices beyond Keyboard.

## References

- [GlobalShortcuts API](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html)
- [RemoteDesktop API](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html)
- [Host application Registry](https://github.com/flatpak/xdg-desktop-portal/blob/1.21.1/data/org.freedesktop.host.portal.Registry.xml)
