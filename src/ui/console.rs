//! The embedded guest screen: the decoded SPICE frame, plus the pointer and
//! keyboard plumbing layered over it.

use gpui::Context;
use gpui::{
    InteractiveElement, IntoElement, MouseButton as GpuiMouseButton, ParentElement, Styled,
    StyledImage, div, img, prelude::FluentBuilder,
};

use gpui::rgb;

use crate::scancode::{
    SPICE_BUTTON_EXTRA, SPICE_BUTTON_LEFT, SPICE_BUTTON_MIDDLE, SPICE_BUTTON_RIGHT,
    SPICE_BUTTON_SIDE,
};
use crate::{IncusManager, theme};

impl IncusManager {
    pub(crate) fn render_console(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = self.active_tab().and_then(|t| t.frame.clone());
        let has_tab = self.active_tab().is_some();
        let connecting = !self.connecting.is_empty();

        div()
            .id("console-surface")
            .track_focus(&self.console_focus)
            .key_context("SpiceConsole")
            .on_key_down(
                cx.listener(|state, event: &gpui::KeyDownEvent, window, cx| {
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
                }),
            )
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
                        .child(div().text_color(theme::dim()).child(if connecting {
                            "正在连接…"
                        } else {
                            "选择一台虚拟机"
                        }))
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
}
