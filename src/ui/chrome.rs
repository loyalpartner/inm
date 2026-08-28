//! The persistent window furniture: the sidebar's project tree, the tab bar
//! that doubles as the titlebar, and the status strip along the bottom.

use gpui::Context;
use gpui::{
    InteractiveElement, IntoElement, MouseButton as GpuiMouseButton, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use gpui::rgb;

use super::{element_id, status_dot};
use crate::{IncusManager, Overlay, PowerAction, Severity, theme, toggle_collapsed};

impl IncusManager {
    pub(crate) fn render_sidebar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                                state.overlay = match state.overlay {
                                    Overlay::RemoteSwitcher => Overlay::None,
                                    _ => Overlay::RemoteSwitcher,
                                };
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
                    .children(self.grouped.iter().map(|(project, vms)| {
                        // A search implicitly expands, otherwise hits stay hidden.
                        let collapsed = !filtering && self.collapsed.contains(project);
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
                                el.children(vms.iter().map(|vm| {
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
                                        .child(status_dot(vm.state))
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
                                        .when(vm.startable(), |el| {
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
                                                            state.power_action(
                                                                id_start.clone(),
                                                                PowerAction::Start,
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            )
                                        })
                                        .on_click(cx.listener(move |state, _, window, cx| {
                                            state.overlay = Overlay::None;
                                            if running {
                                                state.open_or_focus(id_open.clone(), window, cx);
                                            }
                                        }))
                                        .on_mouse_down(
                                            GpuiMouseButton::Right,
                                            cx.listener(move |state, event: &gpui::MouseDownEvent, _, cx| {
                                                state.overlay = Overlay::ContextMenu {
                                                    id: id_menu.clone(),
                                                    at: event.position,
                                                    power_menu_open: false,
                                                };
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
    pub(crate) fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                let members = self.tabs.iter().filter(|t| t.id.project == project).count();
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
            let badge =
                (self.held_modifiers.platform && index < 9).then(|| format!("⌘{}", index + 1));

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
                    .border_color(if is_active {
                        color
                    } else {
                        theme::tint(color, 0.35)
                    })
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
                            // Swallow the press, not just the click. A click
                            // is armed on mouse-down and fired on mouse-up, so
                            // stopping propagation in the click handler alone
                            // relies on the row's own click losing that race —
                            // which it did on macOS but not on Linux, where
                            // closing a tab immediately reconnected the
                            // console. Eating the mouse-down means the row
                            // never arms, whatever the platform does with the
                            // mouse-up.
                            .on_mouse_down(GpuiMouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(move |state, _, _, cx| {
                                cx.stop_propagation();
                                state.close_tab(&id_close);
                                cx.notify();
                            })),
                    )
                    .on_click(cx.listener(move |state, _, window, cx| {
                        // Deliberately not `open_or_focus`: a tab row exists
                        // only for a tab that is already connected, so
                        // clicking one must never be able to *start* a
                        // console — that is what turned a stray click into a
                        // reconnect of the tab being closed.
                        state.focus_tab(&id_focus, window, cx);
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

    pub(crate) fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .children(self.messages.iter().map(|message| {
                        div()
                            .text_xs()
                            .text_color(match message.severity {
                                Severity::Error => theme::danger(),
                                Severity::Notice => theme::accent(),
                            })
                            .child(message.text.clone())
                    }))
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
