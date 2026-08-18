//! Keyboard scancodes and SPICE mouse-button protocol constants — kept
//! together so both stay easy to test and to keep in sync with each other.
//!
//! - [`keyboard`]: gpui key name -> PC XT ("set 1") scancode.
//! - [`dvorak`]: Dvorak-host character -> the QWERTY physical key that typed it.
//! - [`mouse`]: SPICE mouse-button numbers and their `button_state` mask bits.

mod dvorak;
mod keyboard;
mod mouse;

pub use keyboard::scancode_for;
pub use mouse::{
    SPICE_BUTTON_EXTRA, SPICE_BUTTON_LEFT, SPICE_BUTTON_MIDDLE, SPICE_BUTTON_RIGHT,
    SPICE_BUTTON_SIDE, SPICE_BUTTON_WHEEL_DOWN, SPICE_BUTTON_WHEEL_UP, button_mask,
};

/// Which layout the *host* keyboard is using.
///
/// gpui reports the character the host layout produced, not the physical key.
/// A remote desktop has to send the physical key position instead: the guest
/// applies its own layout, and translating twice yields garbage. For a
/// non-QWERTY host we therefore map the reported character back to the
/// QWERTY key position it was typed on.
#[derive(Clone, Copy, PartialEq)]
pub enum HostLayout {
    Qwerty,
    Dvorak,
}

impl HostLayout {
    /// Detect the layout the host is actually using, with `INM_LAYOUT` as an
    /// override.
    ///
    /// This must not depend on the environment alone: launched from Finder,
    /// the Dock, or a bare `./inm`, an env var is simply absent, and the
    /// silent fallback to QWERTY makes every keystroke arrive as a different
    /// letter on a Dvorak host.
    pub fn detect() -> Self {
        if let Ok(forced) = std::env::var("INM_LAYOUT") {
            return match forced.to_lowercase().as_str() {
                "dvorak" => HostLayout::Dvorak,
                _ => HostLayout::Qwerty,
            };
        }
        if dvorak::host_is_dvorak() {
            HostLayout::Dvorak
        } else {
            HostLayout::Qwerty
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HostLayout::Qwerty => "QWERTY",
            HostLayout::Dvorak => "Dvorak",
        }
    }
}

/// Scancode for a key as reported by gpui, accounting for the host layout.
///
/// `shift_held` is the host's *actual* current Shift state (from
/// `sync_modifiers`'s own tracked `ModifiersChanged` stream, not this
/// keystroke's own `modifiers.shift`, which gpui already cleared for
/// anything but a-z — see `keyboard::scancode_for`'s doc comment on the
/// shifted table). It disambiguates the numpad: a bare numpad `+`/`*` press
/// reports the same `key` string as main-row Shift-`=`/Shift-`8`, with no way
/// to tell them apart from the string alone.
pub fn scancode_for_host(key: &str, layout: HostLayout, shift_held: bool) -> Option<u32> {
    if !shift_held {
        match key {
            "+" => return Some(keyboard::KP_PLUS),
            "*" => return Some(keyboard::KP_ASTERISK),
            _ => {}
        }
    }
    let physical = match layout {
        HostLayout::Qwerty => key,
        HostLayout::Dvorak => dvorak::dvorak_to_physical(key),
    };
    scancode_for(physical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dvorak_shifted_symbol_resolves_to_the_physical_key_that_types_it() {
        // Dvorak's "," key sits at the QWERTY "w" position; its shift layer
        // types "<". The physical key sent to the guest must be "w"'s
        // (0x11), not the QWERTY comma key's shifted scancode.
        assert_eq!(
            scancode_for_host("<", HostLayout::Dvorak, true),
            scancode_for("w"),
        );
        // Dvorak's "-" key sits at the QWERTY "'" position; shifted, "_".
        assert_eq!(
            scancode_for_host("_", HostLayout::Dvorak, true),
            scancode_for("'"),
        );
    }

    #[test]
    fn numpad_plus_and_asterisk_are_disambiguated_from_shifted_main_row() {
        // A bare numpad press (no real Shift held) must use the numpad's own
        // dedicated scancode, not the main-row Shift-=/Shift-8 one — sending
        // that would need a Shift the guest never sees held.
        assert_eq!(scancode_for_host("+", HostLayout::Qwerty, false), Some(0x4E));
        assert_eq!(scancode_for_host("*", HostLayout::Qwerty, false), Some(0x37));
        // A real Shift-=/Shift-8 combo keeps resolving to the shifted table.
        assert_eq!(
            scancode_for_host("+", HostLayout::Qwerty, true),
            scancode_for("+"),
        );
        assert_eq!(
            scancode_for_host("*", HostLayout::Qwerty, true),
            scancode_for("*"),
        );
    }
}
