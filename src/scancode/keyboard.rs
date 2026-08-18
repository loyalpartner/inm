//! Map gpui key names onto PC XT ("set 1") scancodes, which is what SPICE's
//! inputs channel expects.
//!
//! Extended keys (arrows, navigation cluster, right-hand modifiers, keypad
//! enter/slash) are the 0xE0-prefixed codes; SPICE carries those as a single
//! value with the prefix in the high byte, i.e. `0xE0 << 8 | code`.

const EXT: u32 = 0xE000;

/// Numpad `+`. Dedicated physical key, no Shift needed to type it — see
/// `super::scancode_for_host`'s doc comment for why it needs its own name
/// rather than living in the shifted-punctuation table below.
pub(super) const KP_PLUS: u32 = 0x4E;
/// Numpad `*`.
pub(super) const KP_ASTERISK: u32 = 0x37;

pub fn scancode_for(key: &str) -> Option<u32> {
    let code = match key {
        // Letters
        "a" => 0x1E, "b" => 0x30, "c" => 0x2E, "d" => 0x20, "e" => 0x12,
        "f" => 0x21, "g" => 0x22, "h" => 0x23, "i" => 0x17, "j" => 0x24,
        "k" => 0x25, "l" => 0x26, "m" => 0x32, "n" => 0x31, "o" => 0x18,
        "p" => 0x19, "q" => 0x10, "r" => 0x13, "s" => 0x1F, "t" => 0x14,
        "u" => 0x16, "v" => 0x2F, "w" => 0x11, "x" => 0x2D, "y" => 0x15,
        "z" => 0x2C,

        // Digits
        "1" => 0x02, "2" => 0x03, "3" => 0x04, "4" => 0x05, "5" => 0x06,
        "6" => 0x07, "7" => 0x08, "8" => 0x09, "9" => 0x0A, "0" => 0x0B,

        // Punctuation
        "-" => 0x0C, "=" => 0x0D, "[" => 0x1A, "]" => 0x1B, "\\" => 0x2B,
        ";" => 0x27, "'" => 0x28, "`" => 0x29, "," => 0x33, "." => 0x34,
        "/" => 0x35,

        // Shifted punctuation. gpui only keeps `modifiers.shift` set for a-z;
        // every other key already carries the shifted glyph with shift
        // cleared (see `Keystroke`'s doc comment), so e.g. shift-1 arrives
        // here as key "!" rather than key "1" with shift held. Map each back
        // to the physical key that types it: the shift itself still reaches
        // the guest, just via `sync_modifiers`'s own ModifiersChanged stream.
        "!" => 0x02, "@" => 0x03, "#" => 0x04, "$" => 0x05, "%" => 0x06,
        "^" => 0x07, "&" => 0x08, "*" => 0x09, "(" => 0x0A, ")" => 0x0B,
        "_" => 0x0C, "+" => 0x0D, "{" => 0x1A, "}" => 0x1B, "|" => 0x2B,
        ":" => 0x27, "\"" => 0x28, "~" => 0x29, "<" => 0x33, ">" => 0x34,
        "?" => 0x35,

        // Whitespace / editing
        "space" => 0x39,
        "enter" => 0x1C,
        "tab" => 0x0F,
        "backspace" => 0x0E,
        "escape" => 0x01,
        "capslock" => 0x3A,
        "scrolllock" => 0x46,

        // Function keys
        "f1" => 0x3B, "f2" => 0x3C, "f3" => 0x3D, "f4" => 0x3E, "f5" => 0x3F,
        "f6" => 0x40, "f7" => 0x41, "f8" => 0x42, "f9" => 0x43, "f10" => 0x44,
        "f11" => 0x57, "f12" => 0x58,
        "f13" => 0x64, "f14" => 0x65, "f15" => 0x66, "f16" => 0x67,
        "f17" => 0x68, "f18" => 0x69, "f19" => 0x6A, "f20" => 0x6B,
        "f21" => 0x6C, "f22" => 0x6D, "f23" => 0x6E, "f24" => 0x76,

        // Modifiers (left-hand variants)
        "shift" => 0x2A,
        "ctrl" | "control" => 0x1D,
        "alt" => 0x38,
        "cmd" | "super" | "win" | "platform" => return Some(EXT | 0x5B),
        "menu" => return Some(EXT | 0x5D),

        // Extended navigation cluster
        "up" => return Some(EXT | 0x48),
        "down" => return Some(EXT | 0x50),
        "left" => return Some(EXT | 0x4B),
        "right" => return Some(EXT | 0x4D),
        "home" => return Some(EXT | 0x47),
        "end" => return Some(EXT | 0x4F),
        "pageup" => return Some(EXT | 0x49),
        "pagedown" => return Some(EXT | 0x51),
        "insert" => return Some(EXT | 0x52),
        "delete" => return Some(EXT | 0x53),

        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::scancode_for;

    #[test]
    fn shifted_symbol_shares_its_number_keys_scancode() {
        assert_eq!(scancode_for("1"), scancode_for("!"));
        assert_eq!(scancode_for("4"), scancode_for("$"));
        assert_eq!(scancode_for(";"), scancode_for(":"));
    }
}
