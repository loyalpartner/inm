//! Everything that draws.
//!
//! Split out of `main.rs`, which had grown to 2800 lines with more than half
//! of it rendering — the state machine and the widgets that show it were
//! interleaved to the point where neither could be read on its own. The
//! `IncusManager` methods here are the same ones as before, just grouped by
//! what they draw.

mod chrome;
mod console;
mod overlays;

use gpui::{IntoElement, SharedString, Styled, div, px};

use crate::incus::{Status, VmId};
use crate::theme;

/// The sidebar's run-state dot. A transitional instance gets its own colour
/// rather than the stopped one — mid-boot is not "off".
pub(crate) fn status_dot(state: Status) -> impl IntoElement {
    div().size(px(6.0)).rounded_full().bg(match state {
        Status::Running => theme::running(),
        Status::Transitional => theme::accent(),
        Status::Stopped | Status::Other => theme::faint(),
    })
}

/// A stable gpui element id for a row/tab belonging to one instance.
pub(crate) fn element_id(id: &VmId, prefix: &str) -> SharedString {
    SharedString::from(format!("{prefix}-{}-{}", id.project, id.name))
}
