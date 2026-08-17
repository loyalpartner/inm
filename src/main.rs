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
use spice_session::InputEvent;
use std::sync::Arc;
use std::time::Duration;

/// SPICE button numbers (spice-protocol SPICE_MOUSE_BUTTON_*).
const SPICE_BUTTON_LEFT: i32 = 1;
const SPICE_BUTTON_RIGHT: i32 = 3;

/// How often the instance list is re-read so status dots stay honest.
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

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
    sidebar_visible: bool,
    /// Modifier state last forwarded to the guest, so releases can be sent as
    /// their own transitions.
    held_modifiers: gpui::Modifiers,
    /// Keys whose press this app consumed, so their release is not forwarded.
    consumed_keys: std::collections::HashSet<String>,
    /// Tab groups (by project) the user folded away.
    collapsed_groups: Vec<SharedString>,
    /// Quick-open palette (⌘P): query plus the highlighted row.
    palette: Option<Palette>,
    palette_focus: gpui::FocusHandle,
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
        self.current_remote = name;
        self.remote_switcher_open = false;
        self.refresh(window, cx);
        cx.notify();
    }

    /// Keep the list fresh without the user having to press anything.
    fn start_auto_refresh(&self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let result = spice_session::runtime()
                    .spawn(incus::list_vms())
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                let alive = this
                    .update(cx, |state, cx| {
                        match result {
                            Ok(vms) => {
                                state.error = None;
                                state.set_vms(vms);
                            }
                            Err(msg) => state.error = Some(msg.into()),
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
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

    fn active_tab(&self) -> Option<&ConsoleTab> {
        let active = self.active.as_ref()?;
        self.tabs.iter().find(|t| &t.id == active)
    }

    fn is_open(&self, id: &VmId) -> bool {
        self.tabs.iter().any(|t| &t.id == id)
    }

    fn close_tab(&mut self, id: &VmId, window: &mut Window) {
        if let Some(pos) = self.tabs.iter().position(|t| &t.id == id) {
            let mut tab = self.tabs.remove(pos);
            // Same reason the frame pump drops the previous image: a
            // RenderImage holds a sprite-atlas slot that is only released
            // explicitly, so closing tabs would otherwise grow it without bound.
            if let Some(frame) = tab.frame.take() {
                let _ = window.drop_image(frame);
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
                            .update_in(cx, |state, window, cx| {
                                let is_active = state.active.as_ref() == Some(&id);
                                let Some(tab) = state.tabs.iter_mut().find(|t| t.id == id) else {
                                    return false;
                                };
                                let previous = tab.frame.replace(image);
                                // Each frame is a distinct RenderImage, so its
                                // atlas entry must be released explicitly or the
                                // sprite atlas grows without bound.
                                if let Some(previous) = previous {
                                    let _ = window.drop_image(previous);
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
        let Some(code) = scancode::scancode_for_host(keystroke.key.as_str(), self.host_layout)
        else {
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
                    self.close_tab(&id, window);
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
                                            if running {
                                                state.open_or_focus(id_open.clone(), window, cx);
                                            }
                                        }))
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
                            .on_click(cx.listener(move |state, _, window, cx| {
                                state.close_tab(&id_close, window);
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
            // gpui does not capture the pointer on press and only fires
            // on_mouse_up while the cursor is still inside, so each button also
            // needs the "released elsewhere" case — otherwise dragging out of
            // the console leaves it held down in the guest.
            .map(|el| {
                [
                    (GpuiMouseButton::Left, SPICE_BUTTON_LEFT),
                    (GpuiMouseButton::Right, SPICE_BUTTON_RIGHT),
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
                        sidebar_visible: true,
                        held_modifiers: gpui::Modifiers::default(),
                        consumed_keys: std::collections::HashSet::new(),
                        collapsed_groups: Vec::new(),
                        palette: None,
                        palette_focus: cx.focus_handle(),
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
                    };
                    state.refresh(window, cx);
                    state.start_auto_refresh(window, cx);
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
