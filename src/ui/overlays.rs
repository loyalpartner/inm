//! The menus and dialogs that float above everything else — see
//! [`crate::Overlay`], which decides which one of these is up at a time.

use gpui::Context;
use gpui::{
    InteractiveElement, IntoElement, MouseButton as GpuiMouseButton, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use super::{element_id, status_dot};
use crate::{IncusManager, MenuAction, Overlay, PowerAction, theme};

impl IncusManager {
    pub(crate) fn render_context_menu(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let Overlay::ContextMenu {
            id,
            at: position,
            power_menu_open,
        } = &self.overlay
        else {
            return None;
        };
        let (id, position, power_menu_open) = (id.clone(), *position, *power_menu_open);
        let vm = self.vms.iter().find(|v| v.id == id)?.clone();
        let running = vm.running();

        // `closes_power_menu` is set for every top-level item except "电源"
        // itself, so hovering a sibling closes its flyout the way a native
        // menu would — only one submenu open at a time.
        let item = |label: SharedString,
                    key: &'static str,
                    enabled: bool,
                    closes_power_menu: bool,
                    action: MenuAction<Self>| {
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
                        if *hovered
                            && let Overlay::ContextMenu {
                                power_menu_open: open @ true,
                                ..
                            } = &mut state.overlay
                        {
                            *open = false;
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
        let submenu_top = power_row_top.min((f32::from(viewport.height) - SUBMENU_SIZE.1).max(0.0));

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
                            state.overlay = Overlay::None;
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
                                        state.overlay = Overlay::None;
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
                                            if *hovered
                                                && let Overlay::ContextMenu {
                                                    power_menu_open: open @ false,
                                                    ..
                                                } = &mut state.overlay
                                            {
                                                *open = true;
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
                                        if running {
                                            "已在运行".into()
                                        } else {
                                            "启动".into()
                                        },
                                        "power-start",
                                        !running,
                                        false,
                                        Box::new(move |state, window, cx| {
                                            state.overlay = Overlay::None;
                                            state.power_action(
                                                id_start.clone(),
                                                PowerAction::Start,
                                                window,
                                                cx,
                                            );
                                        }),
                                    ))
                                    .child(item(
                                        "停止".into(),
                                        "power-stop",
                                        running,
                                        false,
                                        Box::new(move |state, window, cx| {
                                            state.overlay = Overlay::None;
                                            state.power_action(
                                                id_stop.clone(),
                                                PowerAction::Stop,
                                                window,
                                                cx,
                                            );
                                        }),
                                    ))
                                    .child(item(
                                        "重启".into(),
                                        "power-restart",
                                        running,
                                        false,
                                        Box::new(move |state, window, cx| {
                                            state.overlay = Overlay::None;
                                            state.power_action(
                                                id_restart.clone(),
                                                PowerAction::Restart,
                                                window,
                                                cx,
                                            );
                                        }),
                                    )),
                            )
                        }),
                ),
        )
    }

    pub(crate) fn render_details(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let Overlay::Details { id, data } = &self.overlay else {
            return None;
        };
        let details = data.as_ref();

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
                            state.overlay = Overlay::None;
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
                                .child(div().text_sm().text_color(theme::text()).child("详细信息"))
                                .child(
                                    div()
                                        .id("close-details")
                                        .cursor_pointer()
                                        .text_xs()
                                        .text_color(theme::faint())
                                        .hover(|s| s.text_color(theme::text()))
                                        .child("✕")
                                        .on_click(cx.listener(|state, _, _, cx| {
                                            state.overlay = Overlay::None;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(body),
                ),
        )
    }

    pub(crate) fn render_rename(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let Overlay::Rename { id, draft } = &self.overlay else {
            return None;
        };
        let (id, name) = (id.clone(), draft.clone());

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
                        .on_key_down(cx.listener(
                            |state, event: &gpui::KeyDownEvent, window, cx| {
                                state.handle_rename_key(&event.keystroke, window, cx);
                            },
                        ))
                        .on_mouse_down_out(cx.listener(|state, _, window, cx| {
                            state.overlay = Overlay::None;
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

    pub(crate) fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let matches = self.palette_matches();
        let palette = self.overlay.palette();
        let selected = palette
            .and_then(|p| p.selected.as_ref())
            .and_then(|id| matches.iter().position(|vm| &vm.id == id))
            .unwrap_or(0);
        let query = palette.map(|p| p.query.clone()).unwrap_or_default();

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
                    .on_key_down(
                        cx.listener(|state, event: &gpui::KeyDownEvent, window, cx| {
                            state.handle_palette_key(&event.keystroke, window, cx);
                        }),
                    )
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
                            .child(status_dot(vm.state))
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
    pub(crate) fn render_remote_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            // Click-away closes.
            .on_mouse_down(
                GpuiMouseButton::Left,
                cx.listener(|state, _, _, cx| {
                    state.overlay = Overlay::None;
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
                            .text_color(if is_current {
                                theme::accent()
                            } else {
                                theme::text()
                            })
                            .hover(|s| s.bg(theme::hover()))
                            .child(name)
                            .on_click(cx.listener(move |state, _, window, cx| {
                                state.switch_remote(name_for_click.clone(), window, cx);
                            }))
                    })),
            )
    }
}
