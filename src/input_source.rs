//! Force the host's text input source to a plain keyboard layout while
//! inm's window is active, and put the user's own back when it isn't.
//!
//! Everything typed into inm — the VM filter, quick-open palette, rename
//! dialog, and every keystroke forwarded into a SPICE console — is meant to
//! land as literal characters. A CJK/etc IME left active would instead
//! swallow them into a candidate-composition window with nothing here to
//! compose against, so focusing inm should behave like focusing a terminal.
//!
//! Selecting an input source is *global* system state, not per-window, so
//! activation saves whatever was in use and deactivation restores it —
//! otherwise cmd-tabbing away would leave the user's IME silently switched
//! off in every other app.
//!
//! macOS only: switching a Linux IME (fcitx5/ibus/...) has no one standard
//! mechanism the way Carbon's Text Input Source Services does.

/// Switch to a plain keyboard layout, remembering what was in use so
/// [`restore`] can put it back.
#[cfg(target_os = "macos")]
pub fn switch_to_ascii_capable() {
    mac::switch_to_ascii_capable();
}

/// Re-select whatever [`switch_to_ascii_capable`] replaced, if anything.
#[cfg(target_os = "macos")]
pub fn restore() {
    mac::restore();
}

#[cfg(not(target_os = "macos"))]
pub fn switch_to_ascii_capable() {}

#[cfg(not(target_os = "macos"))]
pub fn restore() {}

#[cfg(target_os = "macos")]
mod mac {
    use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
    use core_foundation_sys::base::{CFComparisonResult, CFRelease, CFTypeRef};
    use core_foundation_sys::number::{CFBooleanGetValue, CFBooleanGetTypeID, CFBooleanRef};
    use core_foundation_sys::string::{CFStringCompare, CFStringRef};
    use std::cell::Cell;
    use std::os::raw::c_void;
    use std::ptr;

    type TisInputSourceRef = *const c_void;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> TisInputSourceRef;
        fn TISCreateInputSourceList(properties: CFTypeRef, include_all_installed: u8) -> CFArrayRef;
        fn TISSelectInputSource(input_source: TisInputSourceRef) -> i32;
        fn TISGetInputSourceProperty(
            input_source: TisInputSourceRef,
            property_key: CFStringRef,
        ) -> CFTypeRef;

        static kTISPropertyInputSourceType: CFStringRef;
        static kTISPropertyInputSourceIsASCIICapable: CFStringRef;
        static kTISPropertyInputSourceIsSelectCapable: CFStringRef;
        static kTISTypeKeyboardLayout: CFStringRef;
    }

    thread_local! {
        /// The source displaced by the last switch, retained so it stays
        /// valid until it is selected again. TIS must be used from the main
        /// thread, which is also the only place these are called from, so a
        /// thread-local needs no locking and cannot leak the pointer to
        /// another thread.
        static DISPLACED: Cell<TisInputSourceRef> = const { Cell::new(ptr::null()) };
    }

    /// Read a documented-boolean property, checking the type rather than
    /// trusting it: `TISGetInputSourceProperty` returns a bare `CFTypeRef`
    /// and hands back NULL for a property a given source does not carry.
    unsafe fn bool_property(source: TisInputSourceRef, key: CFStringRef) -> bool {
        let value = unsafe { TISGetInputSourceProperty(source, key) };
        if value.is_null() {
            return false;
        }
        unsafe {
            if core_foundation_sys::base::CFGetTypeID(value) != CFBooleanGetTypeID() {
                return false;
            }
            CFBooleanGetValue(value as CFBooleanRef)
        }
    }

    unsafe fn is_plain_keyboard_layout(source: TisInputSourceRef) -> bool {
        let kind = unsafe { TISGetInputSourceProperty(source, kTISPropertyInputSourceType) };
        if kind.is_null() {
            return false;
        }
        // `kTISPropertyInputSourceIsASCIICapable` alone is the wrong test:
        // the Character Palette and Press-And-Hold both report it, and so do
        // plenty of IMEs (that property means "can produce ASCII", which is
        // exactly what an IME's Roman mode does). Only the *type* separates a
        // real layout from an input method.
        unsafe {
            CFStringCompare(kind as CFStringRef, kTISTypeKeyboardLayout, 0)
                == CFComparisonResult::EqualTo
        }
    }

    pub fn switch_to_ascii_capable() {
        unsafe {
            let current = TISCopyCurrentKeyboardInputSource();
            if current.is_null() {
                return;
            }
            if is_plain_keyboard_layout(current) {
                // Already typing literal characters; nothing to displace, and
                // nothing to restore later.
                CFRelease(current as CFTypeRef);
                return;
            }

            // Every currently *enabled* source. The first enabled plain
            // layout is whatever the user themselves switches to for Latin
            // input (US, ABC, Dvorak, ...), so this never hard-codes one.
            let list = TISCreateInputSourceList(ptr::null(), 0);
            if list.is_null() {
                CFRelease(current as CFTypeRef);
                return;
            }
            let mut switched = false;
            for i in 0..CFArrayGetCount(list) {
                let candidate = CFArrayGetValueAtIndex(list, i) as TisInputSourceRef;
                if !is_plain_keyboard_layout(candidate)
                    || !bool_property(candidate, kTISPropertyInputSourceIsSelectCapable)
                    || !bool_property(candidate, kTISPropertyInputSourceIsASCIICapable)
                {
                    continue;
                }
                // Selection can be refused (an MDM policy, a source that is
                // enabled but not currently selectable) — keep looking rather
                // than silently leaving the IME in place.
                if TISSelectInputSource(candidate) == 0 {
                    switched = true;
                    break;
                }
            }
            CFRelease(list as CFTypeRef);

            if switched {
                // Hold the displaced source alive until it is restored,
                // replacing (and releasing) any earlier one.
                remember(current);
            } else {
                CFRelease(current as CFTypeRef);
            }
        }
    }

    pub fn restore() {
        let previous = DISPLACED.with(|slot| slot.replace(ptr::null()));
        if previous.is_null() {
            return;
        }
        unsafe {
            TISSelectInputSource(previous);
            CFRelease(previous as CFTypeRef);
        }
    }

    /// Take ownership of `source` as the thing to restore later — the +1 it
    /// already carries from `TISCopyCurrentKeyboardInputSource` is the one
    /// `restore` releases, so no extra retain is needed here.
    unsafe fn remember(source: TisInputSourceRef) {
        let stale = DISPLACED.with(|slot| slot.replace(source));
        if !stale.is_null() {
            unsafe { CFRelease(stale as CFTypeRef) };
        }
    }
}
