//! Force the host's text input source back to a plain ASCII-capable
//! keyboard layout whenever inm's window becomes active.
//!
//! Everything typed into inm — the VM filter, quick-open palette, rename
//! dialog, and every keystroke forwarded into a SPICE console — is meant to
//! land as literal characters. A CJK/etc IME left active would instead
//! swallow them into a candidate-composition window with nothing here to
//! compose against, so focusing inm should behave like focusing a terminal.
//!
//! macOS only: switching a Linux IME (fcitx5/ibus/...) has no one standard
//! mechanism the way Carbon's Text Input Source Services does.

#[cfg(target_os = "macos")]
pub fn switch_to_ascii_capable() {
    mac::switch_to_ascii_capable();
}

#[cfg(not(target_os = "macos"))]
pub fn switch_to_ascii_capable() {}

#[cfg(target_os = "macos")]
mod mac {
    use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
    use core_foundation_sys::base::{CFRelease, CFTypeRef};
    use core_foundation_sys::number::{CFBooleanGetValue, CFBooleanRef};
    use core_foundation_sys::string::CFStringRef;
    use std::os::raw::c_void;
    use std::ptr;

    type TisInputSourceRef = *const c_void;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> TisInputSourceRef;
        fn TISCreateInputSourceList(properties: CFTypeRef, include_all_installed: u8) -> CFArrayRef;
        fn TISSelectInputSource(input_source: TisInputSourceRef) -> i32;
        fn TISGetInputSourceProperty(input_source: TisInputSourceRef, property_key: CFStringRef) -> CFTypeRef;

        static kTISPropertyInputSourceIsASCIICapable: CFStringRef;
        static kTISPropertyInputSourceIsSelectCapable: CFStringRef;
    }

    unsafe fn bool_property(source: TisInputSourceRef, key: CFStringRef) -> bool {
        let value = unsafe { TISGetInputSourceProperty(source, key) };
        if value.is_null() {
            return false;
        }
        unsafe { CFBooleanGetValue(value as CFBooleanRef) }
    }

    pub fn switch_to_ascii_capable() {
        unsafe {
            let current = TISCopyCurrentKeyboardInputSource();
            if current.is_null() {
                return;
            }
            let already_ascii = bool_property(current, kTISPropertyInputSourceIsASCIICapable);
            CFRelease(current as CFTypeRef);
            if already_ascii {
                return;
            }

            // Every currently *enabled* source (keyboard layouts and IMEs
            // alike, in the user's own Input Sources order) — the first one
            // that's both selectable and ASCII-capable is whatever plain
            // layout the user already reaches for themselves (US, Dvorak,
            // ABC, ...), so this never has to hard-code a specific layout.
            let list = TISCreateInputSourceList(ptr::null(), 0);
            if list.is_null() {
                return;
            }
            for i in 0..CFArrayGetCount(list) {
                let source = CFArrayGetValueAtIndex(list, i) as TisInputSourceRef;
                if bool_property(source, kTISPropertyInputSourceIsSelectCapable)
                    && bool_property(source, kTISPropertyInputSourceIsASCIICapable)
                {
                    TISSelectInputSource(source);
                    break;
                }
            }
            CFRelease(list as CFTypeRef);
        }
    }
}
