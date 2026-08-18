//! SPICE mouse-button protocol numbers and their `button_state` mask bits
//! (spice-protocol `enums.h`). The two enumerations don't line up — e.g.
//! right is button number 3 but mask bit `1<<2` — so keep them paired here
//! rather than let call sites reconstruct the mapping by hand.

pub const SPICE_BUTTON_LEFT: i32 = 1;
pub const SPICE_BUTTON_MIDDLE: i32 = 2;
pub const SPICE_BUTTON_RIGHT: i32 = 3;
pub const SPICE_BUTTON_WHEEL_UP: i32 = 4;
pub const SPICE_BUTTON_WHEEL_DOWN: i32 = 5;
// "Side"/"extra" are SPICE's names for the two extra buttons past the
// wheel — conventionally the back/forward buttons on mice that have them.
pub const SPICE_BUTTON_SIDE: i32 = 6;
pub const SPICE_BUTTON_EXTRA: i32 = 7;

/// SPICE_MOUSE_BUTTON_MASK_* for a SPICE_MOUSE_BUTTON_* button number.
/// Motion and position events must carry this: it is how the guest's
/// pointer device knows a button is still down mid-drag. Sending 0 there (as
/// opposed to just on button_press/release) makes a held-button drag look to
/// the guest like the button released the instant the mouse moves — window
/// drags and click-drag text selection both silently stop working, while
/// plain clicks still land fine.
pub fn button_mask(button: i32) -> i32 {
    match button {
        1 => 1 << 0, // LEFT
        2 => 1 << 1, // MIDDLE
        3 => 1 << 2, // RIGHT
        4 => 1 << 3, // UP (wheel)
        5 => 1 << 4, // DOWN (wheel)
        6 => 1 << 5, // SIDE (back)
        7 => 1 << 6, // EXTRA (forward)
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_button_has_a_distinct_mask_bit() {
        let buttons = [
            SPICE_BUTTON_LEFT,
            SPICE_BUTTON_MIDDLE,
            SPICE_BUTTON_RIGHT,
            SPICE_BUTTON_WHEEL_UP,
            SPICE_BUTTON_WHEEL_DOWN,
            SPICE_BUTTON_SIDE,
            SPICE_BUTTON_EXTRA,
        ];
        let mut seen = 0;
        for button in buttons {
            let mask = button_mask(button);
            assert_ne!(mask, 0, "button {button} has no mask bit");
            assert_eq!(seen & mask, 0, "button {button}'s mask bit is reused");
            seen |= mask;
        }
    }
}
