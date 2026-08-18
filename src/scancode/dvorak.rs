//! Detecting a Dvorak host layout and translating the characters it reports
//! back to the QWERTY physical key position that produced them.

fn probe(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "macos")]
pub(super) fn host_is_dvorak() -> bool {
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
pub(super) fn host_is_dvorak() -> bool {
    probe("setxkbmap", &["-query"])
        .or_else(|| probe("localectl", &["status"]))
        .is_some_and(|query| active_group_is_dvorak(&query))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn host_is_dvorak() -> bool {
    false
}

/// Does the *first* configured X11 group use Dvorak?
///
/// Both `setxkbmap -query` and `localectl status` list every configured group
/// on one line (`layout: us,us` / `variant: ,dvorak`), but only the first is
/// active at login. Matching "dvorak" anywhere in the output would misread the
/// very common "US plus a Dvorak alternative" setup as a Dvorak host.
///
/// Only reachable on Linux, but compiled (and unit-tested) everywhere so the
/// parsing cannot rot unnoticed on the platform it is developed on.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

/// Reported character -> the QWERTY key position that produced it on a US
/// Dvorak layout. Digits and unlisted keys share their QWERTY position.
pub(super) fn dvorak_to_physical(key: &str) -> &str {
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

        // Shifted punctuation. Same reasoning as `keyboard::scancode_for`'s
        // own shifted table: gpui reports these with `modifiers.shift`
        // cleared, so they must resolve to a physical position on their own.
        // Each maps to the same QWERTY position as its unshifted sibling
        // above — the shift bit still reaches the guest via `sync_modifiers`.
        "\"" => "q", "<" => "w", ">" => "e", "?" => "[", "+" => "]",
        ":" => "z", "_" => "'", "{" => "-", "}" => "=",

        other => other,
    }
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
