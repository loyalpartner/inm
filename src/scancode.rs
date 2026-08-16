//! Map gpui key names onto PC XT ("set 1") scancodes, which is what SPICE's
//! inputs channel expects.
//!
//! Extended keys (arrows, navigation cluster, right-hand modifiers, keypad
//! enter/slash) are the 0xE0-prefixed codes; SPICE carries those as a single
//! value with the prefix in the high byte, i.e. `0xE0 << 8 | code`.

const EXT: u32 = 0xE000;

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
        if host_is_dvorak() {
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

fn probe(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "macos")]
fn host_is_dvorak() -> bool {
    // e.g. "com.apple.keylayout.Dvorak"
    probe(
        "defaults",
        &[
            "read",
            "com.apple.HIToolbox",
            "AppleCurrentKeyboardLayoutInputSourceID",
        ],
    )
    .is_some_and(|id| id.to_lowercase().contains("dvorak"))
}

#[cfg(target_os = "linux")]
fn host_is_dvorak() -> bool {
    probe("setxkbmap", &["-query"])
        .or_else(|| probe("localectl", &["status"]))
        .is_some_and(|query| active_group_is_dvorak(&query))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn host_is_dvorak() -> bool {
    false
}

/// Does the *first* configured X11 group use Dvorak?
///
/// Both `setxkbmap -query` and `localectl status` list every configured group
/// on one line (`layout: us,us` / `variant: ,dvorak`), but only the first is
/// active at login. Matching "dvorak" anywhere in the output would misread the
/// very common "US plus a Dvorak alternative" setup as a Dvorak host.
fn active_group_is_dvorak(query: &str) -> bool {
    let field = |name: &str| -> Option<String> {
        query
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.trim().to_lowercase().ends_with(name))
            .map(|(_, value)| {
                value
                    .trim()
                    .split(',')
                    .next()
                    .unwrap_or_default()
                    .to_lowercase()
            })
    };

    field("layout").is_some_and(|v| v.contains("dvorak"))
        || field("variant").is_some_and(|v| v.contains("dvorak"))
}

#[cfg(test)]
mod tests {
    use super::active_group_is_dvorak;

    #[test]
    fn setxkbmap_dvorak_as_the_first_variant() {
        let query = "rules:      evdev\nmodel:      pc105\nlayout:     us,us\nvariant:    dvorak,\n";
        assert!(active_group_is_dvorak(query));
    }

    #[test]
    fn setxkbmap_dvorak_only_as_the_secondary_group() {
        // The layout the guest VMs use: US first, Dvorak as an alternative.
        let query = "rules:      evdev\nmodel:      pc105\nlayout:     us,us\nvariant:    ,dvorak\n";
        assert!(!active_group_is_dvorak(query));
    }

    #[test]
    fn setxkbmap_plain_dvorak_layout() {
        assert!(active_group_is_dvorak("layout:     dvorak\n"));
    }

    #[test]
    fn localectl_shape() {
        let status = "   System Locale: LANG=en_US.UTF-8\n       VC Keymap: n/a\n      X11 Layout: dvorak\n";
        assert!(active_group_is_dvorak(status));
    }

    #[test]
    fn plain_qwerty() {
        assert!(!active_group_is_dvorak("layout:     us\nvariant:\n"));
    }
}

/// Reported character -> the QWERTY key position that produced it on a US
/// Dvorak layout. Digits and unlisted keys share their QWERTY position.
fn dvorak_to_physical(key: &str) -> &str {
    match key {
        // Top letter row (QWERTY q..\)
        "'" => "q", "," => "w", "." => "e", "p" => "r", "y" => "t",
        "f" => "y", "g" => "u", "c" => "i", "r" => "o", "l" => "p",
        "/" => "[", "=" => "]",

        // Home row (QWERTY a..')
        "a" => "a", "o" => "s", "e" => "d", "u" => "f", "i" => "g",
        "d" => "h", "h" => "j", "t" => "k", "n" => "l", "s" => ";",
        "-" => "'",

        // Bottom row (QWERTY z../)
        ";" => "z", "q" => "x", "j" => "c", "k" => "v", "x" => "b",
        "b" => "n", "m" => "m", "w" => ",", "v" => ".", "z" => "/",

        // Number-row punctuation that Dvorak moves
        "[" => "-", "]" => "=",

        other => other,
    }
}

/// Scancode for a key as reported by gpui, accounting for the host layout.
pub fn scancode_for_host(key: &str, layout: HostLayout) -> Option<u32> {
    let physical = match layout {
        HostLayout::Qwerty => key,
        HostLayout::Dvorak => dvorak_to_physical(key),
    };
    scancode_for(physical)
}

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

        // Whitespace / editing
        "space" => 0x39,
        "enter" => 0x1C,
        "tab" => 0x0F,
        "backspace" => 0x0E,
        "escape" => 0x01,
        "capslock" => 0x3A,

        // Function keys
        "f1" => 0x3B, "f2" => 0x3C, "f3" => 0x3D, "f4" => 0x3E, "f5" => 0x3F,
        "f6" => 0x40, "f7" => 0x41, "f8" => 0x42, "f9" => 0x43, "f10" => 0x44,
        "f11" => 0x57, "f12" => 0x58,

        // Modifiers (left-hand variants)
        "shift" => 0x2A,
        "ctrl" | "control" => 0x1D,
        "alt" => 0x38,
        "cmd" | "super" | "win" | "platform" => return Some(EXT | 0x5B),

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
