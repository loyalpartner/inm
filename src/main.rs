mod incus;
mod incus_remote;
mod scancode;
mod spice_session;

use gpui::{
    div, img, prelude::FluentBuilder, px, rgb, size, App, AppContext, Application, Bounds, Context,
    InteractiveElement, IntoElement, MouseButton as GpuiMouseButton, ParentElement, PromptLevel,
    Render, RenderImage, SharedString, StatefulInteractiveElement, Styled, StyledImage, Window,
    WindowBounds, WindowOptions,
};
use incus::{Vm, VmId};
use scancode::{
    SPICE_BUTTON_EXTRA, SPICE_BUTTON_LEFT, SPICE_BUTTON_MIDDLE, SPICE_BUTTON_RIGHT,
    SPICE_BUTTON_SIDE, SPICE_BUTTON_WHEEL_DOWN, SPICE_BUTTON_WHEEL_UP,
};
use spice_session::InputEvent;
use std::sync::Arc;
use std::time::Duration;

/// Pixel distance treated as one wheel notch, for the precise `Pixels` delta
/// a trackpad reports.
const SCROLL_NOTCH: f32 = 24.0;

/// `ScrollDelta::Lines` magnitude for one physical wheel notch. gpui's own
/// Linux backends (x11 and wayland) hard-code a `SCROLL_LINES = 3.0`
/// multiplier onto every discrete wheel event, so one click there reports
/// 3.0, not 1.0 as on macOS — without correcting for it, Linux scrolls 3x too
/// fast.
#[cfg(target_os = "linux")]
const SCROLL_LINES_PER_NOTCH: f32 = 3.0;
#[cfg(not(target_os = "linux"))]
const SCROLL_LINES_PER_NOTCH: f32 = 1.0;

mod theme {
    use gpui::{rgb, Rgba};

    pub fn bg() -> Rgba {
        rgb(0x1e1e1e)
    }
    pub fn panel() -> Rgba {
        rgb(0x252526)
    }
    pub fn border() -> Rgba {
        rgb(0x3c3c3c)
    }
    pub fn text() -> Rgba {
        rgb(0xd4d4d4)
    }
    pub fn dim() -> Rgba {
        rgb(0x8a8a8a)
    }
    pub fn faint() -> Rgba {
        rgb(0x6e7681)
    }
    pub fn accent() -> Rgba {
        rgb(0x0e93d8)
    }
    pub fn running() -> Rgba {
        rgb(0x3fb950)
    }
    pub fn danger() -> Rgba {
        rgb(0xf14c4c)
    }
    pub fn selected() -> Rgba {
        rgb(0x37373d)
    }
    pub fn hover() -> Rgba {
        rgb(0x2d2d2e)
    }

    /// The same colour at a given alpha, for group fills.
    pub fn tint(color: Rgba, alpha: f32) -> Rgba {
        Rgba { a: alpha, ..color }
    }

    /// A stable colour per project, so a group keeps its identity across
    /// restarts and never depends on tab order.
    pub fn group(project: &str) -> Rgba {
        const PALETTE: [u32; 8] = [
            0x5b9bd5, // blue
            0x57a773, // green
            0xd08770, // orange
            0xb48ead, // purple
            0xd6a44c, // yellow
            0xc9737e, // red
            0x4fb0c6, // cyan
            0x9aa0a6, // grey
        ];
        let hash = project.bytes().fold(0usize, |acc, b| acc.wrapping_mul(31).wrapping_add(b as usize));
        rgb(PALETTE[hash % PALETTE.len()])
    }
}

/// The running/stopped indicator shared by the sidebar and the palette.
fn status_dot(running: bool) -> impl IntoElement {
    div().size(px(6.0)).rounded_full().bg(if running {
        theme::running()
    } else {
        theme::faint()
    })
}

/// Fold or unfold `key` in a collapsed-set.
fn toggle_collapsed(list: &mut Vec<SharedString>, key: &SharedString) {
    match list.iter().position(|p| p == key) {
        Some(pos) => {
            list.remove(pos);
        }
        None => list.push(key.clone()),
    }
}

/// A stable gpui element id for a row/tab belonging to one instance.
fn element_id(id: &VmId, prefix: &str) -> SharedString {
    SharedString::from(format!("{prefix}-{}-{}", id.project, id.name))
}

/// One open console tab: a live SPICE session plus its latest frame. Tabs stay
/// connected while in the background, which is the point — switching back is
/// instant instead of a fresh connect.
struct ConsoleTab {
    id: VmId,
    handle: spice_session::ConsoleHandle,
    frame: Option<Arc<RenderImage>>,
    /// Leftover fractional wheel movement below one notch, per tab so
    /// switching tabs mid-scroll doesn't carry another guest's remainder.
    scroll_remainder: f32,
}

impl ConsoleTab {
    /// Guest resolution, straight from the frame — no second copy to keep in
    /// sync with it.
    fn frame_size(&self) -> Option<(u32, u32)> {
        let size = self.frame.as_ref()?.size(0);
        Some((u32::from(size.width), u32::from(size.height)))
    }
}

/// Quick-open state: a substring query over "project/name" plus the row the
/// arrow keys have landed on.
struct Palette {
    query: String,
    /// The highlighted VM itself. An index would silently point at a different
    /// machine when the background refresh reorders the matches.
    selected: Option<VmId>,
}

/// Last laid-out bounds of the console element, recorded during paint so
/// mouse events can be mapped from window space into guest space.
#[derive(Clone, Default)]
struct ConsoleBounds(Arc<std::sync::Mutex<Option<gpui::Bounds<gpui::Pixels>>>>);

struct IncusManager {
    vms: Vec<Vm>,
    /// Open console tabs, in tab-bar order.
    tabs: Vec<ConsoleTab>,
    /// Which tab is shown; also the target for keyboard and mouse input.
    active: Option<VmId>,
    /// Instances currently being connected, shown as pending tabs so a click
    /// gives immediate feedback.
    connecting: Vec<VmId>,
    collapsed: Vec<SharedString>,
    filter: String,
    /// `vms` grouped by project and filtered, cached because `render` runs on
    /// every delivered video frame while this only changes on a refresh or a
    /// filter edit.
    grouped: Vec<(SharedString, Vec<Vm>)>,
    error: Option<SharedString>,
    /// Transient status-bar message for a lifecycle event (e.g. a VM someone
    /// else just created), cleared a few seconds after it's shown.
    notice: Option<SharedString>,
    sidebar_visible: bool,
    /// Modifier state last forwarded to the guest, so releases can be sent as
    /// their own transitions.
    held_modifiers: gpui::Modifiers,
    /// Host Caps Lock LED state last seen, so a flip can be forwarded as a
    /// single tap (Caps Lock arrives as a level in `ModifiersChangedEvent`,
    /// not as its own key-down/up pair).
    held_capslock: bool,
    /// Keys whose press this app consumed, so their release is not forwarded.
    consumed_keys: std::collections::HashSet<String>,
    /// Tab groups (by project) the user folded away.
    collapsed_groups: Vec<SharedString>,
    /// Open right-click menu: which instance, and where to draw it.
    context_menu: Option<(VmId, gpui::Point<gpui::Pixels>)>,
    /// Whether the context menu's "电源" flyout (启动/停止/重启) is open.
    power_menu_open: bool,
    /// Details dialog: the instance, and its data once it has loaded.
    details: Option<(VmId, Option<incus::VmDetails>)>,
    /// Rename dialog: the instance being renamed and the name being typed.
    rename: Option<(VmId, String)>,
    rename_focus: gpui::FocusHandle,
    /// Quick-open palette (⌘P): query plus the highlighted row.
    palette: Option<Palette>,
    palette_focus: gpui::FocusHandle,
    /// Frames whose GPU texture must not be destroyed yet.
    ///
    /// `Window::drop_image` destroys the atlas texture immediately, with no
    /// fence — safe on Metal, which keeps resources alive until the command
    /// buffer finishes, but on Vulkan it frees memory the GPU may still be
    /// reading for the frame in flight. That faults the device, and gpui's
    /// renderer then spins forever in `while !wait_for(...) {}`, wedging the
    /// whole window. Holding each retired frame for a few repaints guarantees
    /// its last use has completed.
    retired_frames: std::collections::VecDeque<(u64, Arc<RenderImage>)>,
    /// Repaint counter the retirement queue is measured against.
    render_seq: u64,
    console_bounds: ConsoleBounds,
    console_focus: gpui::FocusHandle,
    filter_focus: gpui::FocusHandle,
    host_layout: scancode::HostLayout,
    /// Remotes read from the `incus` CLI's own `config.yml` (only the ones
    /// that are actual Incus servers, not plain image servers).
    remotes: Vec<SharedString>,
    /// Which remote is currently active — starts at `config.yml`'s
    /// `default-remote`, changeable from the sidebar header.
    current_remote: SharedString,
    remote_switcher_open: bool,
    /// Bumped on every remote switch so a still-running event listener from
    /// the previous remote knows to stop instead of reporting events for a
    /// remote that is no longer current.
    remote_epoch: u64,
}

impl IncusManager {
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            // `incus::list_vms` dials a Unix socket via tokio, so it has to
            // run on the tokio runtime rather than gpui's own executor.
            let result = spice_session::runtime()
                .spawn(incus::list_vms())
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |state, cx| {
                match result {
                    Ok(vms) => {
                        state.error = None;
                        state.set_vms(vms);
                    }
                    // Keep whatever list is already on screen rather than
                    // silently blanking it on a transient failure (a flaky
                    // remote during auto-refresh, say) — just say why it's
                    // stale instead of leaving the list looking "empty" for
                    // no visible reason.
                    Err(msg) => state.error = Some(msg.into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Switch which Incus remote the app talks to (mirrors `incus remote
    /// switch`, just from inside the app). There's no simultaneous
    /// multi-remote view, so this drops whatever the previous remote had
    /// open and starts fresh.
    fn switch_remote(&mut self, name: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(msg) = incus_remote::switch_to(&name) {
            self.error = Some(msg.into());
            cx.notify();
            return;
        }

        for mut tab in self.tabs.drain(..) {
            if let Some(frame) = tab.frame.take() {
                let _ = window.drop_image(frame);
            }
            tab.handle.stop();
        }
        self.active = None;
        self.connecting.clear();
        self.vms.clear();
        self.grouped.clear();
        self.error = None;
        self.notice = None;
        self.current_remote = name;
        self.remote_switcher_open = false;
        self.remote_epoch += 1;
        self.refresh(window, cx);
        self.start_event_listener(window, cx);
        cx.notify();
    }

    /// Keep the list fresh without the user having to press anything: an
    /// instance changing anywhere (created, started, stopped, deleted,
    /// renamed, ...) pushes a refresh over the daemon's own event stream, so
    /// there is no polling interval to lag behind. `instance-created`
    /// additionally surfaces a status-bar notice, since that one is easy to
    /// otherwise miss in a scrolling list.
    fn start_event_listener(&self, window: &mut Window, cx: &mut Context<Self>) {
        let epoch = self.remote_epoch;
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<incus::InstanceEvent>();

        // The websocket read loop lives entirely on the tokio runtime — same
        // split as the SPICE frame pump in `spice_session`, since gpui's own
        // executor cannot poll a tokio I/O type directly. A dropped
        // `JoinHandle` does not stop the task; it keeps forwarding events
        // for the life of the process, or until `tx` has no more receivers.
        spice_session::runtime().spawn(async move {
            loop {
                if let Ok(mut ws) = incus::events_websocket().await {
                    while let Some(event) = incus::next_instance_event(&mut ws).await {
                        if tx.unbounded_send(event).is_err() {
                            return;
                        }
                    }
                }
                // The daemon doesn't support the endpoint, or the connection
                // dropped — either way, back off instead of hot-looping.
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        cx.spawn_in(window, async move |this, cx| {
            use futures::StreamExt;
            while let Some(event) = rx.next().await {
                let alive = this
                    .update_in(cx, |state, window, cx| {
                        // A newer listener has since taken over for a
                        // different remote; let this one die quietly.
                        if state.remote_epoch != epoch {
                            return false;
                        }
                        if event.action == "instance-created" {
                            state.notice =
                                Some(format!("已创建虚拟机 {}/{}", event.id.project, event.id.name).into());
                            state.clear_notice_after(Duration::from_secs(5), window, cx);
                        }
                        state.refresh(window, cx);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Clear `notice` after `delay`, but only if nothing newer has replaced
    /// it in the meantime.
    fn clear_notice_after(&self, delay: Duration, window: &mut Window, cx: &mut Context<Self>) {
        let showing = self.notice.clone();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |state, cx| {
                if state.notice == showing {
                    state.notice = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Switch the visible console. Kept in one place so the "only the shown
    /// tab converts frames" invariant cannot drift between call sites.
    fn set_active(&mut self, id: Option<VmId>) {
        self.active = id;
        for tab in &self.tabs {
            tab.handle.set_visible(self.active.as_ref() == Some(&tab.id));
        }
    }

    /// Close the topmost overlay, if any. Returns whether something closed.
    fn dismiss_overlay(&mut self, window: &mut Window) -> bool {
        if self.context_menu.take().is_some() {
            return true;
        }
        if self.details.take().is_some() || self.rename.take().is_some() {
            window.focus(&self.console_focus);
            return true;
        }
        false
    }

    fn begin_rename(&mut self, id: VmId, window: &mut Window, cx: &mut Context<Self>) {
        self.context_menu = None;
        self.rename = Some((id.clone(), id.name.to_string()));
        window.focus(&self.rename_focus);
        cx.notify();
    }

    fn handle_rename_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((id, name)) = self.rename.as_mut() else {
            return;
        };
        match keystroke.key.as_str() {
            "escape" => {
                self.rename = None;
                window.focus(&self.console_focus);
            }
            "backspace" => {
                name.pop();
            }
            "enter" => {
                let (id, name) = (id.clone(), name.trim().to_string());
                if name.is_empty() || name == id.name.as_ref() {
                    self.rename = None;
                    window.focus(&self.console_focus);
                } else {
                    self.commit_rename(id, name, window, cx);
                }
            }
            _ => {
                if let Some(ch) = keystroke.key_char.as_ref() {
                    if ch.chars().all(|c| !c.is_control()) {
                        name.push_str(ch);
                    }
                }
            }
        }
        cx.notify();
    }

    fn commit_rename(
        &mut self,
        id: VmId,
        new_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rename = None;
        window.focus(&self.console_focus);
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let (target, name) = (id.clone(), new_name.clone());
            let result = spice_session::runtime()
                .spawn(async move { incus::rename(&target, &name).await })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            this.update_in(cx, |state, window, cx| {
                if let Err(msg) = result {
                    state.error = Some(msg.into());
                }
                state.refresh(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn show_details(&mut self, id: VmId, window: &mut Window, cx: &mut Context<Self>) {
        self.context_menu = None;
        self.details = Some((id.clone(), None));
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let fetch_id = id.clone();
            let result = spice_session::runtime()
                .spawn(async move { incus::details(&fetch_id).await })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |state, cx| {
                match result {
                    // Only fill in if the dialog is still showing this VM —
                    // the user may have closed it or opened another meanwhile.
                    Ok(details) if state.details.as_ref().is_some_and(|(d, _)| d == &id) => {
                        state.details = Some((id.clone(), Some(details)));
                    }
                    Ok(_) => {}
                    Err(msg) => {
                        // Same identity guard as the success arm: a slow
                        // failure for VM A must not close the dialog the user
                        // has since opened for VM B.
                        if state.details.as_ref().is_some_and(|(d, _)| d == &id) {
                            state.details = None;
                        }
                        state.error = Some(msg.into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Hand a frame over for delayed destruction.
    fn retire_frame(&mut self, frame: Arc<RenderImage>) {
        self.retired_frames.push_back((self.render_seq, frame));
    }

    /// Destroy the textures of frames that can no longer be in flight.
    fn release_retired_frames(&mut self, window: &mut Window) {
        // Two completed repaints after a frame's last use is comfortably past
        // anything the renderer can still hold: it waits for the previous
        // frame's fence before recording the next one.
        const KEEP_FOR_REPAINTS: u64 = 2;
        while let Some((seq, _)) = self.retired_frames.front() {
            if self.render_seq.saturating_sub(*seq) < KEEP_FOR_REPAINTS {
                break;
            }
            if let Some((_, frame)) = self.retired_frames.pop_front() {
                let _ = window.drop_image(frame);
            }
        }
    }

    fn active_tab(&self) -> Option<&ConsoleTab> {
        let active = self.active.as_ref()?;
        self.tabs.iter().find(|t| &t.id == active)
    }

    fn active_tab_mut(&mut self) -> Option<&mut ConsoleTab> {
        let active = self.active.clone()?;
        self.tabs.iter_mut().find(|t| t.id == active)
    }

    fn is_open(&self, id: &VmId) -> bool {
        self.tabs.iter().any(|t| &t.id == id)
    }

    fn close_tab(&mut self, id: &VmId) {
        if let Some(pos) = self.tabs.iter().position(|t| &t.id == id) {
            let mut tab = self.tabs.remove(pos);
            // Same reason the frame pump retires images: the atlas slot has to
            // be released explicitly, but only once the GPU can no longer be
            // reading it.
            if let Some(frame) = tab.frame.take() {
                self.retire_frame(frame);
            }
            tab.handle.stop();
            if self.active.as_ref() == Some(id) {
                // Fall back to the neighbour that took its place.
                let next = self
                    .tabs
                    .get(pos)
                    .or_else(|| self.tabs.last())
                    .map(|t| t.id.clone());
                self.set_active(next);
            }
        }
    }

    /// Open `id` in a tab, or just switch to it when it is already connected.
    fn open_or_focus(&mut self, id: VmId, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        window.focus(&self.console_focus);

        if self.is_open(&id) {
            self.set_active(Some(id));
            cx.notify();
            return;
        }
        if self.connecting.contains(&id) {
            return;
        }
        self.connecting.push(id.clone());
        cx.notify();
        self.connect_console(id, false, window, cx);
    }

    fn connect_console(
        &mut self,
        id: VmId,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let task = spice_session::runtime().spawn(spice_session::start_console(
                id.name.to_string(),
                id.project.to_string(),
                force,
            ));
            let result = match task.await {
                Ok(r) => r,
                Err(join_err) => Err(spice_session::StartError::Message(join_err.to_string())),
            };

            match result {
                Ok((handle, mut frames)) => {
                    this.update_in(cx, |state, window, cx| {
                        state.connecting.retain(|c| c != &id);
                        let tab = ConsoleTab {
                            id: id.clone(),
                            handle,
                            frame: None,
                            scroll_remainder: 0.0,
                        };
                        // Keep a project's tabs adjacent so groups stay
                        // contiguous, the way Chrome moves a tab into its
                        // group rather than leaving it stranded.
                        match state
                            .tabs
                            .iter()
                            .rposition(|t| t.id.project == id.project)
                        {
                            Some(last) => state.tabs.insert(last + 1, tab),
                            None => state.tabs.push(tab),
                        }
                        state.set_active(Some(id.clone()));
                        window.focus(&state.console_focus);
                        cx.notify();
                    })
                    .ok();

                    use futures::StreamExt;
                    while let Some(mut image) = frames.next().await {
                        // Only the newest frame is worth painting; uploading
                        // every queued one just burns GPU time to show images
                        // that are already stale.
                        #[allow(deprecated)] // try_recv returns Result<T, _>; this needs "is one ready?"
                        while let Ok(Some(newer)) = frames.try_next() {
                            image = newer;
                        }

                        let id = id.clone();
                        let still_open = this
                            .update(cx, |state, cx| {
                                let is_active = state.active.as_ref() == Some(&id);
                                let Some(tab) = state.tabs.iter_mut().find(|t| t.id == id) else {
                                    return false;
                                };
                                let previous = tab.frame.replace(image);
                                // Each frame is a distinct RenderImage, so its
                                // atlas entry must be released explicitly or the
                                // sprite atlas grows without bound — but not
                                // before the GPU is done with it.
                                if let Some(previous) = previous {
                                    state.retire_frame(previous);
                                }
                                // Background tabs keep decoding (that is what
                                // keeps them warm) but must not force repaints.
                                if is_active {
                                    cx.notify();
                                }
                                true
                            })
                            .unwrap_or(false);
                        if !still_open {
                            break;
                        }
                    }

                    // The stream ends both when close_tab() already tore the
                    // console down (expected — the tab is gone by now, do
                    // nothing) and when the connection dropped out from under
                    // a still-open tab (unexpected — say so, instead of
                    // leaving a frozen console with no explanation).
                    this.update(cx, |state, cx| {
                        if state.is_open(&id) {
                            state.close_tab(&id);
                            state.error = Some(format!("{} 的控制台连接已断开", id.name).into());
                            cx.notify();
                        }
                    })
                    .ok();
                }
                Err(spice_session::StartError::AlreadyConnected) => {
                    let answer = cx.prompt(
                        PromptLevel::Warning,
                        &spice_session::StartError::AlreadyConnected.message(),
                        Some("是否强制接管？"),
                        &["取消", "强制接管"],
                    );
                    let take_over = matches!(answer.await, Ok(1));
                    this.update_in(cx, |state, window, cx| {
                        if take_over {
                            state.connect_console(id.clone(), true, window, cx);
                        } else {
                            state.connecting.retain(|c| c != &id);
                        }
                        cx.notify();
                    })
                    .ok();
                }
                Err(err) => {
                    this.update(cx, |state, cx| {
                        state.connecting.retain(|c| c != &id);
                        state.error = Some(err.message().into());
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    fn start_vm(&mut self, id: VmId, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let result = spice_session::runtime()
                .spawn(async move { incus::start(&id).await })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            this.update_in(cx, |state, window, cx| {
                if let Err(msg) = result {
                    state.error = Some(msg.into());
                }
                state.refresh(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Stopping (or restarting) a VM with an open console tab drops that
    /// tab's SPICE connection out from under it; the frame stream ending
    /// unexpectedly is exactly what the "disconnected" handling in
    /// `connect_console` already surfaces to the user, so there is nothing
    /// extra to do here for that case.
    fn stop_vm(&mut self, id: VmId, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let result = spice_session::runtime()
                .spawn(async move { incus::stop(&id).await })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            this.update_in(cx, |state, window, cx| {
                if let Err(msg) = result {
                    state.error = Some(msg.into());
                }
                state.refresh(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn restart_vm(&mut self, id: VmId, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let result = spice_session::runtime()
                .spawn(async move { incus::restart(&id).await })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            this.update_in(cx, |state, window, cx| {
                if let Err(msg) = result {
                    state.error = Some(msg.into());
                }
                state.refresh(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Map a window-space mouse position onto guest coordinates. The image is
    /// letterboxed (object-fit: contain), so undo that centering and scaling.
    fn send_mouse_motion(&self, position: gpui::Point<gpui::Pixels>) {
        let Some(tab) = self.active_tab() else { return };
        let Some((fw, fh)) = tab.frame_size() else {
            return;
        };
        let Some(bounds) = *self.console_bounds.0.lock().unwrap() else {
            return;
        };

        let bw = f32::from(bounds.size.width);
        let bh = f32::from(bounds.size.height);
        if bw <= 0.0 || bh <= 0.0 {
            return;
        }

        let local_x = f32::from(position.x) - f32::from(bounds.origin.x);
        let local_y = f32::from(position.y) - f32::from(bounds.origin.y);

        let scale = (bw / fw as f32).min(bh / fh as f32);
        let offset_x = (bw - fw as f32 * scale) / 2.0;
        let offset_y = (bh - fh as f32 * scale) / 2.0;

        let gx = ((local_x - offset_x) / scale).round();
        let gy = ((local_y - offset_y) / scale).round();
        if gx < 0.0 || gy < 0.0 || gx >= fw as f32 || gy >= fh as f32 {
            return;
        }

        tab.handle.send_input(InputEvent::MouseMotion {
            x: gx as i32,
            y: gy as i32,
        });
    }

    fn send_mouse_button(&self, button: i32, pressed: bool) {
        if let Some(tab) = self.active_tab() {
            tab.handle
                .send_input(InputEvent::MouseButton { button, pressed });
        }
    }

    /// SPICE carries the wheel as discrete button-4/5 clicks, not a
    /// continuous delta, so accumulate movement and emit whole notches.
    fn send_mouse_scroll(&mut self, event: &gpui::ScrollWheelEvent) {
        let lines = match event.delta {
            gpui::ScrollDelta::Pixels(delta) => f32::from(delta.y) / SCROLL_NOTCH,
            gpui::ScrollDelta::Lines(delta) => delta.y / SCROLL_LINES_PER_NOTCH,
        };

        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        tab.scroll_remainder += lines;
        let notches = tab.scroll_remainder.trunc();
        tab.scroll_remainder -= notches;

        let button = if notches > 0.0 {
            SPICE_BUTTON_WHEEL_UP
        } else {
            SPICE_BUTTON_WHEEL_DOWN
        };
        // Cap so a single huge trackpad flick cannot flood the guest.
        for _ in 0..(notches.abs() as u32).min(10) {
            tab.handle.send_input(InputEvent::MouseScroll { button });
        }
    }

    /// Ctrl+Alt+Del cannot be typed on a Mac keyboard, so offer it explicitly.
    fn send_ctrl_alt_del(&self) {
        let Some(tab) = self.active_tab() else { return };
        let Some(combo) = ["control", "alt", "delete"]
            .into_iter()
            .map(scancode::scancode_for)
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        for code in combo.iter().copied() {
            tab.handle.send_input(InputEvent::KeyPress(code));
        }
        for code in combo.iter().rev() {
            tab.handle.send_input(InputEvent::KeyRelease(*code));
        }
    }

    /// Forward one key transition to the guest.
    ///
    /// Modifiers are deliberately *not* bracketed around the key here. gpui
    /// reports modifier presses and releases on a separate event stream
    /// (`ModifiersChanged`), so deriving them from whichever ordinary key
    /// happens to be in flight loses the release whenever the user lets go of
    /// the modifier first — leaving it stuck down in the guest. `sync_modifiers`
    /// owns that state instead.
    fn send_key(&self, keystroke: &gpui::Keystroke, pressed: bool) {
        let Some(tab) = self.active_tab() else { return };
        let Some(code) = scancode::scancode_for_host(
            keystroke.key.as_str(),
            self.host_layout,
            self.held_modifiers.shift,
        ) else {
            return;
        };
        tab.handle.send_input(if pressed {
            InputEvent::KeyPress(code)
        } else {
            InputEvent::KeyRelease(code)
        });
    }

    /// Mirror host modifier transitions into the guest.
    ///
    /// Cmd is intentionally never forwarded: it is this app's own modifier, and
    /// sending Super to a Linux guest would pop its overview on every shortcut.
    fn sync_modifiers(&mut self, modifiers: gpui::Modifiers) {
        let before = self.held_modifiers;
        self.held_modifiers = modifiers;

        let Some(tab) = self.active_tab() else { return };
        for (now, was, key) in [
            (modifiers.control, before.control, "control"),
            (modifiers.alt, before.alt, "alt"),
            (modifiers.shift, before.shift, "shift"),
        ] {
            let Some(code) = scancode::scancode_for(key) else {
                continue;
            };
            if now && !was {
                tab.handle.send_input(InputEvent::KeyPress(code));
            } else if !now && was {
                tab.handle.send_input(InputEvent::KeyRelease(code));
            }
        }
    }

    /// Mirror a host Caps Lock toggle into the guest.
    ///
    /// Unlike the other modifiers, Caps Lock is a *level* in gpui's own
    /// `ModifiersChangedEvent` (its LED state), not a press/release pair —
    /// physical hardware toggles it the same way, though: each press sends a
    /// normal make+break scancode, and the keyboard controller's own LED
    /// logic (host and guest alike) is what flips the lock. So on a change,
    /// tap the key rather than trying to hold it down.
    fn sync_capslock(&mut self, capslock: gpui::Capslock) {
        let before = self.held_capslock;
        self.held_capslock = capslock.on;
        if capslock.on == before {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let Some(code) = scancode::scancode_for("capslock") else {
            return;
        };
        tab.handle.send_input(InputEvent::KeyPress(code));
        tab.handle.send_input(InputEvent::KeyRelease(code));
    }

    /// App-level shortcuts. Cmd is the modifier because Linux guests almost
    /// never need Super combos, so nothing useful is stolen from them.
    /// Returns true when the key was consumed and must not reach the guest.
    fn handle_shortcut(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !keystroke.modifiers.platform {
            return false;
        }
        match keystroke.key.as_str() {
            "w" => {
                if let Some(id) = self.active.clone() {
                    self.close_tab(&id);
                    cx.notify();
                }
            }
            "r" => self.refresh(window, cx),
            "b" => {
                self.sidebar_visible = !self.sidebar_visible;
                cx.notify();
            }
            "f" => {
                self.sidebar_visible = true;
                window.focus(&self.filter_focus);
                cx.notify();
            }
            "p" => self.open_palette(window, cx),
            digit @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") => {
                let index = digit.parse::<usize>().unwrap_or(1) - 1;
                if let Some(id) = self.tabs.get(index).map(|t| t.id.clone()) {
                    self.set_active(Some(id));
                    window.focus(&self.console_focus);
                    cx.notify();
                }
            }
            _ => return false,
        }
        true
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = Some(Palette {
            query: String::new(),
            selected: None,
        });
        window.focus(&self.palette_focus);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        window.focus(&self.console_focus);
        cx.notify();
    }

    /// Rows shown in the palette: every VM whose project or name contains the
    /// query, running ones first so the common case is one keystroke away.
    fn palette_matches(&self) -> Vec<Vm> {
        let Some(palette) = &self.palette else {
            return Vec::new();
        };
        let needle = palette.query.to_lowercase();
        let mut matches: Vec<Vm> = self
            .vms
            .iter()
            .filter(|vm| {
                needle.is_empty()
                    || vm.id.name.to_lowercase().contains(&needle)
                    || vm.id.project.to_lowercase().contains(&needle)
            })
            .cloned()
            .collect();
        matches.sort_by_key(|vm| !vm.running());
        matches.truncate(12);
        matches
    }

    fn handle_palette_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let matches = self.palette_matches();
        let Some(palette) = self.palette.as_mut() else {
            return;
        };

        // Resolve the highlight against the list as it stands right now, so a
        // background refresh that reorders or removes rows cannot leave the
        // selection pointing at a machine the user never highlighted.
        let current = palette
            .selected
            .as_ref()
            .and_then(|id| matches.iter().position(|vm| &vm.id == id))
            .unwrap_or(0);
        let last = matches.len().saturating_sub(1);
        let select = |index: usize, palette: &mut Palette| {
            palette.selected = matches.get(index).map(|vm| vm.id.clone());
        };

        // Readline-style navigation alongside the arrows: ⌃P/⌃N, and ⌘P
        // cycles down so holding Cmd and tapping P walks the list.
        if keystroke.modifiers.control || keystroke.modifiers.platform {
            match keystroke.key.as_str() {
                "p" if keystroke.modifiers.control => {
                    select(current.saturating_sub(1), palette)
                }
                "n" if keystroke.modifiers.control => select((current + 1).min(last), palette),
                // ⌘P again: wrap around rather than stopping at the bottom.
                "p" => select(if current >= last { 0 } else { current + 1 }, palette),
                "u" if keystroke.modifiers.control => {
                    palette.query.clear();
                    palette.selected = None;
                }
                _ => return,
            }
            cx.notify();
            return;
        }

        match keystroke.key.as_str() {
            "escape" => {
                self.close_palette(window, cx);
                return;
            }
            "backspace" => {
                palette.query.pop();
                palette.selected = None;
            }
            "down" => select((current + 1).min(last), palette),
            "up" => select(current.saturating_sub(1), palette),
            "enter" => {
                let chosen = palette
                    .selected
                    .as_ref()
                    .and_then(|id| matches.iter().find(|vm| &vm.id == id).cloned())
                    .or_else(|| matches.first().cloned());
                if let Some(vm) = chosen {
                    self.close_palette(window, cx);
                    if vm.running() {
                        self.open_or_focus(vm.id, window, cx);
                    } else {
                        self.error = Some("该虚拟机未运行".into());
                    }
                }
                return;
            }
            _ => {
                if let Some(ch) = keystroke.key_char.as_ref() {
                    if ch.chars().all(|c| !c.is_control()) {
                        palette.query.push_str(ch);
                        palette.selected = None;
                    }
                }
            }
        }
        cx.notify();
    }

    fn render_context_menu(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let (id, position) = self.context_menu.clone()?;
        let vm = self.vms.iter().find(|v| v.id == id)?.clone();
        let running = vm.running();
        let power_menu_open = self.power_menu_open;

        // `closes_power_menu` is set for every top-level item except "电源"
        // itself, so hovering a sibling closes its flyout the way a native
        // menu would — only one submenu open at a time.
        let item = |label: SharedString,
                    key: &'static str,
                    enabled: bool,
                    closes_power_menu: bool,
                    action: Box<dyn Fn(&mut Self, &mut Window, &mut Context<Self>)>| {
            div()
                .id(SharedString::from(format!("menu-{key}")))
                .px_3()
                .py_1()
                .text_sm()
                .when(enabled, |s| {
                    s.cursor_pointer()
                        .text_color(theme::text())
                        .hover(|s| s.bg(theme::selected()))
                })
                .when(!enabled, |s| s.text_color(theme::faint()))
                .child(label)
                .when(closes_power_menu, |s| {
                    s.on_hover(cx.listener(|state, hovered: &bool, _, cx| {
                        if *hovered && state.power_menu_open {
                            state.power_menu_open = false;
                            cx.notify();
                        }
                    }))
                })
                .on_click(cx.listener(move |state, _, window, cx| {
                    if enabled {
                        action(state, window, cx);
                    }
                }))
        };

        let id_console = id.clone();
        let id_details = id.clone();
        let id_rename = id.clone();
        let id_start = id.clone();
        let id_stop = id.clone();
        let id_restart = id.clone();

        // Keep the menu on screen: anchored at the pointer it would otherwise
        // hang off the bottom for rows near the end of a long sidebar, leaving
        // its last items clipped and unclickable.
        const MENU_SIZE: (f32, f32) = (170.0, 116.0);
        // Rows are text_sm + px_3/py_1; four of them plus one divider add up
        // to MENU_SIZE.1 above, so this is that same per-row figure.
        const ROW_HEIGHT: f32 = 28.0;
        const SUBMENU_SIZE: (f32, f32) = (110.0, 92.0);

        let viewport = window.viewport_size();
        let left = f32::from(position.x).min((f32::from(viewport.width) - MENU_SIZE.0).max(0.0));
        let top = f32::from(position.y).min((f32::from(viewport.height) - MENU_SIZE.1).max(0.0));

        // "电源" is the second row, right below "打开控制台", so its flyout
        // hangs off that row rather than the menu's top edge. It opens to
        // whichever side still fits, mirroring the edge-avoidance above.
        let power_row_top = top + ROW_HEIGHT;
        let submenu_left = if left + MENU_SIZE.0 + SUBMENU_SIZE.0 <= f32::from(viewport.width) {
            left + MENU_SIZE.0
        } else {
            (left - SUBMENU_SIZE.0).max(0.0)
        };
        let submenu_top =
            power_row_top.min((f32::from(viewport.height) - SUBMENU_SIZE.1).max(0.0));

        // The menu and its flyout are two visually separate panels but must
        // share one hit-test region: `on_mouse_down_out` below fires on any
        // press outside that region's own bounds (regardless of DOM
        // nesting), so if the flyout's screen area were not part of it, a
        // press on a flyout item would be seen as "outside", dismiss the
        // menu, and eat the press before the item's own click ever fires.
        let (union_left, union_top, union_right, union_bottom) = if power_menu_open {
            (
                left.min(submenu_left),
                top.min(submenu_top),
                (left + MENU_SIZE.0).max(submenu_left + SUBMENU_SIZE.0),
                (top + MENU_SIZE.1).max(submenu_top + SUBMENU_SIZE.1),
            )
        } else {
            (left, top, left + MENU_SIZE.0, top + MENU_SIZE.1)
        };

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                // Swallow clicks aimed at dismissing the menu, so they do not
                // also land on the row or console underneath.
                .occlude()
                .child(
                    div()
                        .id("context-menu")
                        // Dismiss on a click *outside* the menu. A full-screen
                        // catcher listening for mouse-down would eat the press
                        // that precedes an item's click, so the item's action
                        // would never run.
                        .on_mouse_down_out(cx.listener(|state, _, _, cx| {
                            state.context_menu = None;
                            cx.notify();
                        }))
                        .absolute()
                        .left(px(union_left))
                        .top(px(union_top))
                        .w(px(union_right - union_left))
                        .h(px(union_bottom - union_top))
                        .child(
                            div()
                                .absolute()
                                .left(px(left - union_left))
                                .top(px(top - union_top))
                                .w(px(MENU_SIZE.0))
                                .py_1()
                                .rounded_md()
                                .bg(theme::panel())
                                .border_1()
                                .border_color(theme::border())
                                .shadow_lg()
                                .flex()
                                .flex_col()
                                .child(item(
                                    "打开控制台".into(),
                                    "console",
                                    running,
                                    true,
                                    Box::new(move |state, window, cx| {
                                        state.context_menu = None;
                                        state.open_or_focus(id_console.clone(), window, cx);
                                    }),
                                ))
                                .child(
                                    div()
                                        .id("menu-power")
                                        .px_3()
                                        .py_1()
                                        .text_sm()
                                        .cursor_pointer()
                                        .text_color(theme::text())
                                        .hover(|s| s.bg(theme::selected()))
                                        .flex()
                                        .flex_row()
                                        .justify_between()
                                        .child("电源")
                                        .child(div().text_color(theme::faint()).child("▸"))
                                        .on_hover(cx.listener(|state, hovered: &bool, _, cx| {
                                            if *hovered && !state.power_menu_open {
                                                state.power_menu_open = true;
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(div().my_1().h(px(1.0)).bg(theme::border()))
                                .child(item(
                                    if running {
                                        "重命名（需先停止）".into()
                                    } else {
                                        "重命名".into()
                                    },
                                    "rename",
                                    !running,
                                    true,
                                    Box::new(move |state, window, cx| {
                                        state.begin_rename(id_rename.clone(), window, cx);
                                    }),
                                ))
                                .child(item(
                                    "详细信息".into(),
                                    "details",
                                    true,
                                    true,
                                    Box::new(move |state, window, cx| {
                                        state.show_details(id_details.clone(), window, cx);
                                    }),
                                )),
                        )
                        .when(power_menu_open, |el| {
                            el.child(
                                div()
                                    .absolute()
                                    .left(px(submenu_left - union_left))
                                    .top(px(submenu_top - union_top))
                                    .w(px(SUBMENU_SIZE.0))
                                    .py_1()
                                    .rounded_md()
                                    .bg(theme::panel())
                                    .border_1()
                                    .border_color(theme::border())
                                    .shadow_lg()
                                    .flex()
                                    .flex_col()
                                    .child(item(
                                        if running { "已在运行".into() } else { "启动".into() },
                                        "power-start",
                                        !running,
                                        false,
                                        Box::new(move |state, window, cx| {
                                            state.context_menu = None;
                                            state.power_menu_open = false;
                                            state.start_vm(id_start.clone(), window, cx);
                                        }),
                                    ))
                                    .child(item(
                                        "停止".into(),
                                        "power-stop",
                                        running,
                                        false,
                                        Box::new(move |state, window, cx| {
                                            state.context_menu = None;
                                            state.power_menu_open = false;
                                            state.stop_vm(id_stop.clone(), window, cx);
                                        }),
                                    ))
                                    .child(item(
                                        "重启".into(),
                                        "power-restart",
                                        running,
                                        false,
                                        Box::new(move |state, window, cx| {
                                            state.context_menu = None;
                                            state.power_menu_open = false;
                                            state.restart_vm(id_restart.clone(), window, cx);
                                        }),
                                    )),
                            )
                        }),
                ),
        )
    }

    fn render_details(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (id, details) = self.details.as_ref()?;

        let row = |label: &'static str, value: String| {
            div()
                .flex()
                .flex_row()
                .gap_3()
                .py_0p5()
                .child(
                    div()
                        .w(px(84.0))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(theme::faint())
                        .child(label),
                )
                .child(div().text_sm().text_color(theme::text()).child(value))
        };

        let mut body = div().flex().flex_col().px_4().py_3().gap_0p5();
        body = body
            .child(row("项目", id.project.to_string()))
            .child(row("名称", id.name.to_string()));

        match details {
            None => body = body.child(row("", "加载中…".into())),
            Some(d) => {
                body = body
                    .child(row("状态", d.status.clone()))
                    // The whole point of this dialog: which cluster member is
                    // actually running the instance.
                    .child(row(
                        "宿主机",
                        match (d.location.is_empty(), &d.location_address) {
                            (true, _) => "（非集群）".to_string(),
                            (false, Some(addr)) => format!("{}  {addr}", d.location),
                            (false, None) => d.location.clone(),
                        },
                    ))
                    .when_some(d.location_status.clone(), |el, status| {
                        el.child(row("节点状态", status))
                    })
                    .child(row("架构", d.architecture.clone()));

                if let Some(cpu) = &d.cpu_limit {
                    body = body.child(row("CPU", cpu.clone()));
                }
                if let Some(mem) = &d.memory_limit {
                    let used = d
                        .memory_usage
                        .map(|b| format!("（已用 {:.1} GiB）", b as f64 / (1 << 30) as f64))
                        .unwrap_or_default();
                    body = body.child(row("内存", format!("{mem}{used}")));
                }
                if let Some(disk) = &d.root_disk {
                    body = body.child(row("根盘", disk.clone()));
                }
                for (iface, ip) in &d.addresses {
                    body = body.child(row("地址", format!("{ip}  ({iface})")));
                }
                if !d.profiles.is_empty() {
                    body = body.child(row("profile", d.profiles.join(", ")));
                }
                if let Some(created) = d.created_at.split('T').next() {
                    body = body.child(row("创建于", created.to_string()));
                }
            }
        }

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .pt_20()
                .occlude()
                .child(
                    div()
                        .id("details-dialog")
                        .on_mouse_down_out(cx.listener(|state, _, _, cx| {
                            state.details = None;
                            cx.notify();
                        }))
                        .w(px(420.0))
                        .rounded_lg()
                        .bg(theme::panel())
                        .border_1()
                        .border_color(theme::border())
                        .shadow_lg()
                        .overflow_hidden()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_between()
                                .items_center()
                                .px_4()
                                .py_2()
                                .border_b_1()
                                .border_color(theme::border())
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme::text())
                                        .child("详细信息"),
                                )
                                .child(
                                    div()
                                        .id("close-details")
                                        .cursor_pointer()
                                        .text_xs()
                                        .text_color(theme::faint())
                                        .hover(|s| s.text_color(theme::text()))
                                        .child("✕")
                                        .on_click(cx.listener(|state, _, _, cx| {
                                            state.details = None;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(body),
                ),
        )
    }

    fn render_rename(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (id, name) = self.rename.clone()?;

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .pt_20()
                .occlude()
                .child(
                    div()
                        .id("rename-dialog")
                        .track_focus(&self.rename_focus)
                        .key_context("Rename")
                        .on_key_down(cx.listener(|state, event: &gpui::KeyDownEvent, window, cx| {
                            state.handle_rename_key(&event.keystroke, window, cx);
                        }))
                        .on_mouse_down_out(cx.listener(|state, _, window, cx| {
                            state.rename = None;
                            window.focus(&state.console_focus);
                            cx.notify();
                        }))
                        .w(px(380.0))
                        .rounded_lg()
                        .bg(theme::panel())
                        .border_1()
                        .border_color(theme::border())
                        .shadow_lg()
                        .overflow_hidden()
                        .child(
                            div()
                                .px_4()
                                .py_2()
                                .border_b_1()
                                .border_color(theme::border())
                                .text_sm()
                                .text_color(theme::text())
                                .child(format!("重命名 {}", id.name)),
                        )
                        .child(
                            div()
                                .m_3()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(theme::bg())
                                .border_1()
                                .border_color(theme::accent())
                                .text_sm()
                                .text_color(theme::text())
                                .child(name),
                        )
                        .child(
                            div()
                                .px_4()
                                .pb_3()
                                .text_xs()
                                .text_color(theme::faint())
                                .child("⏎ 确认 · Esc 取消"),
                        ),
                ),
        )
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let matches = self.palette_matches();
        let selected = self
            .palette
            .as_ref()
            .and_then(|p| p.selected.as_ref())
            .and_then(|id| matches.iter().position(|vm| &vm.id == id))
            .unwrap_or(0);
        let query = self
            .palette
            .as_ref()
            .map(|p| p.query.clone())
            .unwrap_or_default();

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt_20()
            // Click-away closes.
            .on_mouse_down(
                GpuiMouseButton::Left,
                cx.listener(|state, _, window, cx| state.close_palette(window, cx)),
            )
            .child(
                div()
                    .id("palette")
                    .track_focus(&self.palette_focus)
                    .key_context("Palette")
                    .on_key_down(cx.listener(|state, event: &gpui::KeyDownEvent, window, cx| {
                        state.handle_palette_key(&event.keystroke, window, cx);
                    }))
                    .w(px(520.0))
                    .flex()
                    .flex_col()
                    .rounded_lg()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .shadow_lg()
                    .overflow_hidden()
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(theme::border())
                            .text_color(if query.is_empty() {
                                theme::faint()
                            } else {
                                theme::text()
                            })
                            .flex()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .child(if query.is_empty() {
                                "跳转到虚拟机…".to_string()
                            } else {
                                query
                            })
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::faint())
                                    .child("⌃P/⌃N 选择 · ⏎ 打开"),
                            ),
                    )
                    .children(matches.into_iter().enumerate().map(|(i, vm)| {
                        let is_selected = i == selected;
                        let running = vm.running();
                        let id = vm.id.clone();

                        div()
                            .id(element_id(&vm.id, "pal"))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_1()
                            .cursor_pointer()
                            .when(is_selected, |s| s.bg(theme::selected()))
                            .hover(|s| s.bg(theme::hover()))
                            .child(status_dot(running))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .text_color(if running { theme::text() } else { theme::dim() })
                                    .child(vm.id.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::faint())
                                    .child(vm.id.project.clone()),
                            )
                            .on_click(cx.listener(move |state, _, window, cx| {
                                state.close_palette(window, cx);
                                if running {
                                    state.open_or_focus(id.clone(), window, cx);
                                }
                            }))
                    })),
            )
    }

    /// A small dropdown under the sidebar header listing every remote from
    /// `config.yml`. Deliberately simpler than the palette — no search, no
    /// keyboard nav — since it's a short, low-frequency list.
    fn render_remote_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            // Click-away closes.
            .on_mouse_down(
                GpuiMouseButton::Left,
                cx.listener(|state, _, _, cx| {
                    state.remote_switcher_open = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    .top(px(40.0))
                    .left(px(12.0))
                    .w(px(180.0))
                    .flex()
                    .flex_col()
                    .py_1()
                    .rounded_lg()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .shadow_lg()
                    .overflow_hidden()
                    .children(self.remotes.iter().cloned().map(|name| {
                        let is_current = name == self.current_remote;
                        let name_for_click = name.clone();
                        div()
                            .id(SharedString::from(format!("remote-{name}")))
                            .px_3()
                            .py_1p5()
                            .text_sm()
                            .cursor_pointer()
                            .text_color(if is_current { theme::accent() } else { theme::text() })
                            .hover(|s| s.bg(theme::hover()))
                            .child(name)
                            .on_click(cx.listener(move |state, _, window, cx| {
                                state.switch_remote(name_for_click.clone(), window, cx);
                            }))
                    })),
            )
    }

    /// Edit the sidebar filter. The box is a plain focusable element rather
    /// than a full text field — it only ever needs append/erase/clear.
    fn handle_filter_key(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        match keystroke.key.as_str() {
            "backspace" => {
                self.filter.pop();
            }
            "escape" => self.filter.clear(),
            _ => {
                if let Some(ch) = keystroke.key_char.as_ref() {
                    if ch.chars().all(|c| !c.is_control()) {
                        self.filter.push_str(ch);
                    }
                }
            }
        }
        self.regroup();
        cx.notify();
    }

    fn set_vms(&mut self, vms: Vec<Vm>) {
        self.vms = vms;
        self.regroup();
    }

    /// Rebuild the sidebar's grouped/filtered view. Called only when `vms` or
    /// `filter` changes — never from `render`.
    fn regroup(&mut self) {
        let needle = self.filter.to_lowercase();
        let mut groups: Vec<(SharedString, Vec<Vm>)> = Vec::new();
        for vm in &self.vms {
            if !needle.is_empty() && !vm.id.name.to_lowercase().contains(&needle) {
                continue;
            }
            match groups.last_mut() {
                Some((project, list)) if *project == vm.id.project => list.push(vm.clone()),
                _ => groups.push((vm.id.project.clone(), vec![vm.clone()])),
            }
        }
        self.grouped = groups;
    }

    fn render_sidebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.vms.len();
        let filtering = !self.filter.is_empty();

        div()
            .flex()
            .flex_col()
            .w(px(232.0))
            // Without this the console image's intrinsic width (the guest
            // resolution) squeezes the sidebar, which also shifts the
            // console's bounds away from where it paints.
            .flex_shrink_0()
            .h_full()
            .bg(theme::panel())
            .border_r_1()
            .border_color(theme::border())
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px_3()
                    .pt_3()
                    .flex_shrink_0()
                    .child(
                        div()
                            .id("remote-switcher-toggle")
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme::dim())
                            .hover(|s| s.text_color(theme::text()))
                            .child(format!("{} ▾", self.current_remote))
                            .on_click(cx.listener(|state, _, _, cx| {
                                state.remote_switcher_open = !state.remote_switcher_open;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("refresh")
                            .cursor_pointer()
                            .px_1()
                            .rounded_md()
                            .text_xs()
                            .text_color(theme::dim())
                            .hover(|s| s.bg(theme::hover()).text_color(theme::text()))
                            .child(format!("{total} 台 ⟳"))
                            .on_click(cx.listener(|state, _, window, cx| {
                                state.refresh(window, cx);
                            })),
                    ),
            )
            .child(
                // Filter box
                div()
                    .id("filter")
                    .track_focus(&self.filter_focus)
                    .key_context("Filter")
                    .on_key_down(cx.listener(|state, event: &gpui::KeyDownEvent, _, cx| {
                        state.handle_filter_key(&event.keystroke, cx);
                    }))
                    .mx_2()
                    .my_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme::bg())
                    .border_1()
                    .border_color(if self.filter_focus.is_focused(window) {
                        theme::accent()
                    } else {
                        theme::border()
                    })
                    .text_xs()
                    .text_color(if filtering { theme::text() } else { theme::faint() })
                    .cursor_pointer()
                    .child(if filtering {
                        self.filter.clone()
                    } else {
                        "搜索  ⌘F".to_string()
                    }),
            )
            .child(
                div()
                    .id("vm-tree")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(self.grouped.iter().cloned().map(|(project, vms)| {
                        // A search implicitly expands, otherwise hits stay hidden.
                        let collapsed = !filtering && self.collapsed.contains(&project);
                        let project_for_click = project.clone();

                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .id(SharedString::from(format!("proj-{project}")))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .px_2()
                                    .py_1()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme::hover()))
                                    .child(
                                        div()
                                            .w(px(10.0))
                                            .text_xs()
                                            .text_color(theme::faint())
                                            .child(if collapsed { "▸" } else { "▾" }),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_xs()
                                            .text_color(theme::dim())
                                            .child(project.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::faint())
                                            .child(format!("{}", vms.len())),
                                    )
                                    .on_click(cx.listener(move |state, _, _, cx| {
                                        toggle_collapsed(&mut state.collapsed, &project_for_click);
                                        cx.notify();
                                    })),
                            )
                            .when(!collapsed, |el| {
                                el.children(vms.into_iter().map(|vm| {
                                    let is_active = self.active.as_ref() == Some(&vm.id);
                                    let is_open = self.is_open(&vm.id);
                                    let pending = self.connecting.contains(&vm.id);
                                    let running = vm.running();
                                    let id_open = vm.id.clone();
                                    let id_start = vm.id.clone();
                                    let id_menu = vm.id.clone();

                                    div()
                                        .id(element_id(&vm.id, "vm"))
                                        .group("row")
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_2()
                                        .pl_5()
                                        .pr_2()
                                        .py_1()
                                        .cursor_pointer()
                                        .when(is_active, |s| s.bg(theme::selected()))
                                        .hover(|s| s.bg(theme::hover()))
                            .child(status_dot(running))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .text_sm()
                                                .text_color(if running {
                                                    theme::text()
                                                } else {
                                                    theme::dim()
                                                })
                                                .child(vm.id.name.clone()),
                                        )
                                        .when(!vm.location.is_empty(), |el| {
                                            el.child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_xs()
                                                    .text_color(theme::faint())
                                                    .child(vm.location.clone()),
                                            )
                                        })
                                        .when(pending, |el| {
                                            el.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme::dim())
                                                    .child("…"),
                                            )
                                        })
                                        .when(is_open && !pending, |el| {
                                            el.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme::accent())
                                                    .child("●"),
                                            )
                                        })
                                        .when(!running, |el| {
                                            // Starting a VM is an explicit act,
                                            // so it gets its own control rather
                                            // than happening on row click.
                                            el.child(
                                                div()
                                                    .id(element_id(&vm.id, "start"))
                                                    .text_xs()
                                                    .text_color(theme::faint())
                                                    .hover(|s| s.text_color(theme::running()))
                                                    .child("▶")
                                                    .on_click(cx.listener(
                                                        move |state, _, window, cx| {
                                                            state.start_vm(
                                                                id_start.clone(),
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            )
                                        })
                                        .on_click(cx.listener(move |state, _, window, cx| {
                                            state.context_menu = None;
                                            if running {
                                                state.open_or_focus(id_open.clone(), window, cx);
                                            }
                                        }))
                                        .on_mouse_down(
                                            GpuiMouseButton::Right,
                                            cx.listener(move |state, event: &gpui::MouseDownEvent, _, cx| {
                                                state.context_menu =
                                                    Some((id_menu.clone(), event.position));
                                                state.power_menu_open = false;
                                                cx.notify();
                                            }),
                                        )
                                }))
                            })
                    })),
            )
    }

    /// The titlebar strip: window chrome, sidebar toggle, and the tabs —
    /// grouped by project the way Chrome groups tabs, since a project is
    /// exactly the "these belong together" boundary here.
    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut current: Option<SharedString> = None;

        for (index, tab) in self.tabs.iter().enumerate() {
            let project = tab.id.project.clone();
            let color = theme::group(&project);
            let is_active = self.active.as_ref() == Some(&tab.id);
            let collapsed = self.collapsed_groups.contains(&project);

            // Group header: a coloured pill introducing the run of tabs.
            if current.as_ref() != Some(&project) {
                current = Some(project.clone());
                let members = self
                    .tabs
                    .iter()
                    .filter(|t| t.id.project == project)
                    .count();
                let project_for_click = project.clone();

                rows.push(
                    div()
                        .id(SharedString::from(format!("group-{project}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .ml_2()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .flex_shrink_0()
                        .cursor_pointer()
                        .bg(theme::tint(color, 0.22))
                        .hover(|s| s.bg(theme::tint(color, 0.34)))
                        .child(div().size(px(6.0)).rounded_full().bg(color))
                        .child(div().text_xs().text_color(color).child(project.clone()))
                        .when(collapsed, |el| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(color)
                                    .child(format!("{members}")),
                            )
                        })
                        .on_click(cx.listener(move |state, _, _, cx| {
                            toggle_collapsed(&mut state.collapsed_groups, &project_for_click);
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }

            // A folded group still shows whichever tab you are looking at.
            if collapsed && !is_active {
                continue;
            }

            let id_focus = tab.id.clone();
            let id_close = tab.id.clone();
            // Hold Cmd and every tab shows the number that selects it.
            let badge = (self.held_modifiers.platform && index < 9).then(|| format!("⌘{}", index + 1));

            rows.push(
                div()
                    .id(element_id(&tab.id, "tab"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h_full()
                    .flex_shrink_0()
                    .cursor_pointer()
                    // The group colour rides along the top edge, so a tab is
                    // readable as "part of that group" at a glance.
                    .border_t_2()
                    .border_color(if is_active { color } else { theme::tint(color, 0.35) })
                    .when(is_active, |s| s.bg(theme::bg()))
                    .when(!is_active, |s| s.hover(|s| s.bg(theme::hover())))
                    .when_some(badge, |el, badge| {
                        el.child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .bg(theme::accent())
                                .text_xs()
                                .text_color(rgb(0xffffff))
                                .child(badge),
                        )
                    })
                    .child(
                        div()
                            .text_sm()
                            .text_color(if is_active {
                                theme::text()
                            } else {
                                theme::dim()
                            })
                            .child(tab.id.name.clone()),
                    )
                    .child(
                        div()
                            .id(element_id(&tab.id, "close"))
                            .text_xs()
                            .text_color(theme::faint())
                            .hover(|s| s.text_color(theme::danger()))
                            .child("✕")
                            .on_click(cx.listener(move |state, _, _, cx| {
                                // Without this the click also lands on the
                                // row's own on_click below, which just
                                // reopened the tab we were closing.
                                cx.stop_propagation();
                                state.close_tab(&id_close);
                                cx.notify();
                            })),
                    )
                    .on_click(cx.listener(move |state, _, window, cx| {
                        state.open_or_focus(id_focus.clone(), window, cx);
                    }))
                    .into_any_element(),
            );
        }

        // Pending tabs, so clicking a VM shows something immediately.
        for id in &self.connecting {
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h_full()
                    .flex_shrink_0()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::faint())
                            .child(id.name.clone()),
                    )
                    .child(div().text_xs().text_color(theme::faint()).child("连接中…"))
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .w_full()
            .h(px(38.0))
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .overflow_hidden()
            // The strip *is* the titlebar, so dragging it moves the window.
            .on_mouse_down(GpuiMouseButton::Left, |_, window, _| {
                window.start_window_move();
            })
            // Mirrors the sidebar's width so the first tab lines up exactly
            // with the console's left edge.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .flex_shrink_0()
                    .when(self.sidebar_visible, |s| {
                        s.w(px(232.0)).border_r_1().border_color(theme::border())
                    })
                    // Leaves the macOS traffic lights their corner.
                    .child(div().w(px(72.0)).h_full().flex_shrink_0())
                    .child(
                        div()
                            .id("toggle-sidebar")
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(34.0))
                            .h_full()
                            .flex_shrink_0()
                            .cursor_pointer()
                    // Drawn rather than typed: a glyph would sit at whatever
                    // weight the system font decides, which never matches a
                    // flat UI. This is a panel outline with its left column
                    // filled — solid when the sidebar is showing.
                    .child(
                        div()
                            .w(px(15.0))
                            .h(px(12.0))
                            .rounded_sm()
                            .border_1()
                            .border_color(if self.sidebar_visible {
                                theme::dim()
                            } else {
                                theme::faint()
                            })
                            .flex()
                            .flex_row()
                            .child(div().w(px(4.0)).h_full().bg(if self.sidebar_visible {
                                theme::dim()
                            } else {
                                theme::faint()
                            })),
                    )
                            .hover(|s| s.bg(theme::hover()))
                            .on_click(cx.listener(|state, _, _, cx| {
                                state.sidebar_visible = !state.sidebar_visible;
                                cx.notify();
                            })),
                    ),
            )
            .children(rows)
    }

    fn render_console(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = self.active_tab().and_then(|t| t.frame.clone());
        let has_tab = self.active_tab().is_some();
        let connecting = !self.connecting.is_empty();

        div()
            .id("console-surface")
            .track_focus(&self.console_focus)
            .key_context("SpiceConsole")
            .on_key_down(cx.listener(|state, event: &gpui::KeyDownEvent, window, cx| {
                // Escape closes whatever overlay is on top. It has to be
                // checked here rather than in handle_shortcut, which only ever
                // runs for Cmd-modified keys — a bare Escape would otherwise
                // sail past and get typed into the guest instead.
                if event.keystroke.key == "escape" && state.dismiss_overlay(window) {
                    cx.notify();
                    return;
                }
                // Anything held with Cmd belongs to the app, whether or not it
                // maps to a shortcut. Forwarding the press but then swallowing
                // the release (which is what happens when the guard is on the
                // *release* side) wedges the key down in the guest.
                if event.keystroke.modifiers.platform {
                    state.consumed_keys.insert(event.keystroke.key.clone());
                    state.handle_shortcut(&event.keystroke, window, cx);
                    return;
                }
                state.send_key(&event.keystroke, true);
            }))
            .on_key_up(cx.listener(|state, event: &gpui::KeyUpEvent, _, _| {
                // Release only what we actually pressed. Matching on the key
                // rather than on the current modifiers keeps the pair balanced
                // even if Cmd was tapped while an ordinary key was held.
                if state.consumed_keys.remove(&event.keystroke.key) {
                    return;
                }
                state.send_key(&event.keystroke, false);
            }))
            .flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(rgb(0x000000))
            .overflow_hidden()
            .relative()
            .child({
                // Records where the console ended up so mouse events can be
                // mapped to guest space.
                let slot = self.console_bounds.clone();
                gpui::canvas(
                    move |bounds, _, _| {
                        *slot.0.lock().unwrap() = Some(bounds);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            .when_some(frame, |el, frame| {
                el.child(img(frame).size_full().object_fit(gpui::ObjectFit::Contain))
            })
            .when(!has_tab, |el| {
                el.child(
                    div()
                        .absolute()
                        .size_full()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_color(theme::dim())
                                .child(if connecting { "正在连接…" } else { "选择一台虚拟机" }),
                        )
                        .when(!connecting, |el| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(theme::faint())
                                    .child("⌘P 跳转 · ⌘F 搜索 · ⌘B 侧栏 · ⌘1…9 切换 · ⌘W 关闭"),
                            )
                        }),
                )
            })
            .on_mouse_move(cx.listener(|state, event: &gpui::MouseMoveEvent, _, _| {
                state.send_mouse_motion(event.position);
            }))
            .on_scroll_wheel(cx.listener(|state, event: &gpui::ScrollWheelEvent, _, _| {
                state.send_mouse_motion(event.position);
                state.send_mouse_scroll(event);
            }))
            .on_mouse_down(
                GpuiMouseButton::Left,
                cx.listener(|state, event: &gpui::MouseDownEvent, window, _| {
                    // Clicking the console takes keyboard focus so typing goes
                    // to the guest, not the app.
                    window.focus(&state.console_focus);
                    state.send_mouse_motion(event.position);
                    state.send_mouse_button(SPICE_BUTTON_LEFT, true);
                }),
            )
            .on_mouse_down(
                GpuiMouseButton::Right,
                cx.listener(|state, event: &gpui::MouseDownEvent, _, _| {
                    state.send_mouse_motion(event.position);
                    state.send_mouse_button(SPICE_BUTTON_RIGHT, true);
                }),
            )
            .on_mouse_down(
                GpuiMouseButton::Middle,
                cx.listener(|state, event: &gpui::MouseDownEvent, _, _| {
                    state.send_mouse_motion(event.position);
                    state.send_mouse_button(SPICE_BUTTON_MIDDLE, true);
                }),
            )
            .on_mouse_down(
                GpuiMouseButton::Navigate(gpui::NavigationDirection::Back),
                cx.listener(|state, event: &gpui::MouseDownEvent, _, _| {
                    state.send_mouse_motion(event.position);
                    state.send_mouse_button(SPICE_BUTTON_SIDE, true);
                }),
            )
            .on_mouse_down(
                GpuiMouseButton::Navigate(gpui::NavigationDirection::Forward),
                cx.listener(|state, event: &gpui::MouseDownEvent, _, _| {
                    state.send_mouse_motion(event.position);
                    state.send_mouse_button(SPICE_BUTTON_EXTRA, true);
                }),
            )
            // gpui does not capture the pointer on press and only fires
            // on_mouse_up while the cursor is still inside, so each button also
            // needs the "released elsewhere" case — otherwise dragging out of
            // the console leaves it held down in the guest.
            .map(|el| {
                [
                    (GpuiMouseButton::Left, SPICE_BUTTON_LEFT),
                    (GpuiMouseButton::Right, SPICE_BUTTON_RIGHT),
                    (GpuiMouseButton::Middle, SPICE_BUTTON_MIDDLE),
                    (
                        GpuiMouseButton::Navigate(gpui::NavigationDirection::Back),
                        SPICE_BUTTON_SIDE,
                    ),
                    (
                        GpuiMouseButton::Navigate(gpui::NavigationDirection::Forward),
                        SPICE_BUTTON_EXTRA,
                    ),
                ]
                .into_iter()
                .fold(el, |el, (gpui_button, spice_button)| {
                    el.on_mouse_up(
                        gpui_button,
                        cx.listener(move |state, _, _, _| {
                            state.send_mouse_button(spice_button, false);
                        }),
                    )
                    .on_mouse_up_out(
                        gpui_button,
                        cx.listener(move |state, _, _, _| {
                            state.send_mouse_button(spice_button, false);
                        }),
                    )
                })
            })
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.active_tab();
        let detail = tab.map(|t| {
            let (w, h) = t.frame_size().unwrap_or((0, 0));
            let pointer = if t.handle.absolute_pointer() {
                "绝对定位"
            } else {
                "相对移动"
            };
            format!(
                "{}  ·  {w}×{h}  ·  {pointer}  ·  {}",
                t.id.project,
                self.host_layout.label()
            )
        });

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .h(px(22.0))
            .px_3()
            .bg(theme::panel())
            .border_t_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_xs()
                    .text_color(theme::faint())
                    .child(detail.unwrap_or_else(|| "未连接".into())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .items_center()
                    .when_some(self.error.clone(), |el, err| {
                        el.child(div().text_xs().text_color(theme::danger()).child(err))
                    })
                    .when_some(self.notice.clone(), |el, notice| {
                        el.child(div().text_xs().text_color(theme::accent()).child(notice))
                    })
                    .when(tab.is_some(), |el| {
                        el.child(
                            div()
                                .id("cad")
                                .text_xs()
                                .cursor_pointer()
                                .text_color(theme::faint())
                                .hover(|s| s.text_color(theme::text()))
                                .child("发送 Ctrl+Alt+Del")
                                .on_click(cx.listener(|state, _, _, _| {
                                    state.send_ctrl_alt_del();
                                })),
                        )
                    }),
            )
    }
}

impl Render for IncusManager {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A repaint is about to happen, so whatever frame the console handed
        // over has been consumed — let its session build the next one. This is
        // what paces frame production to the renderer instead of to a timer.
        if let Some(tab) = self.active_tab() {
            tab.handle.frame_painted();
        }
        self.render_seq = self.render_seq.wrapping_add(1);
        self.release_retired_frames(window);

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .on_modifiers_changed(cx.listener(|state, event: &gpui::ModifiersChangedEvent, _, cx| {
                // The Cmd badges on tabs are driven by this same state, so a
                // change in it is exactly when the tab bar needs repainting.
                let had_cmd = state.held_modifiers.platform;
                state.sync_modifiers(event.modifiers);
                state.sync_capslock(event.capslock);
                if had_cmd != event.modifiers.platform {
                    cx.notify();
                }
            }))
            .child(self.render_tab_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .when(self.sidebar_visible, |el| {
                        el.child(self.render_sidebar(window, cx))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            // Let this pane be narrower than the image's
                            // intrinsic size instead of forcing the layout
                            // wider.
                            .min_w_0()
                            .overflow_hidden()
                            .h_full()
                            .child(self.render_console(cx)),
                    ),
            )
            .child(self.render_status_bar(cx))
            .when(self.palette.is_some(), |el| el.child(self.render_palette(cx)))
            .children(self.render_context_menu(window, cx))
            .children(self.render_details(cx))
            .children(self.render_rename(cx))
            .when(self.remote_switcher_open, |el| {
                el.child(self.render_remote_switcher(cx))
            })
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // Draw our own title area: the tab bar doubles as the
                // titlebar, so the system chrome would only waste a strip.
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("inm".into()),
                    appears_transparent: true,
                    // Clear of the traffic lights, vertically centred in the
                    // 38px tab strip.
                    traffic_light_position: Some(gpui::point(px(13.0), px(13.0))),
                }),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    let mut state = IncusManager {
                        vms: Vec::new(),
                        tabs: Vec::new(),
                        active: None,
                        connecting: Vec::new(),
                        collapsed: Vec::new(),
                        filter: String::new(),
                        grouped: Vec::new(),
                        error: None,
                        notice: None,
                        sidebar_visible: true,
                        held_modifiers: gpui::Modifiers::default(),
                        held_capslock: false,
                        consumed_keys: std::collections::HashSet::new(),
                        collapsed_groups: Vec::new(),
                        context_menu: None,
                        power_menu_open: false,
                        details: None,
                        rename: None,
                        rename_focus: cx.focus_handle(),
                        palette: None,
                        palette_focus: cx.focus_handle(),
                        retired_frames: std::collections::VecDeque::new(),
                        render_seq: 0,
                        console_bounds: ConsoleBounds::default(),
                        console_focus: cx.focus_handle(),
                        filter_focus: cx.focus_handle(),
                        host_layout: scancode::HostLayout::detect(),
                        remotes: incus_remote::list_remotes()
                            .unwrap_or_default()
                            .into_iter()
                            .map(Into::into)
                            .collect(),
                        current_remote: incus_remote::current_name().into(),
                        remote_switcher_open: false,
                        remote_epoch: 0,
                    };
                    state.refresh(window, cx);
                    state.start_event_listener(window, cx);
                    // Focus the console up front so the shortcuts work before
                    // anything has been opened.
                    window.focus(&state.console_focus);
                    state
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
