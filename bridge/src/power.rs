//! Keep macOS from idle-sleeping while a buddy is connected.
//!
//! When the laptop's display sleeps, macOS normally follows with system idle
//! sleep, which powers down the Bluetooth radio and tears down our BLE link —
//! the buddy "disconnects when the screen goes dark". To prevent that we hold an
//! `IOPMAssertion` of type `PreventUserIdleSystemSleep` for the life of each
//! connection.
//!
//! This is *not* a settings change: the assertion is a runtime, process-scoped
//! request (the same mechanism `caffeinate -i` and media playback use). Nothing
//! in System Settings is touched, and it evaporates the instant the guard drops
//! (disconnect, daemon exit, or crash). It blocks only *idle* system sleep — the
//! display still dims/sleeps on its normal timer, and a deliberate sleep (lid
//! close, Apple menu → Sleep) still sleeps the machine and drops the link.
//!
//! On non-macOS targets every entry point is a no-op so the crate still builds
//! and runs unchanged.

/// An RAII guard that holds a system-sleep-preventing power assertion for as
/// long as it is alive. Acquire one on connect; drop it on disconnect.
pub struct PowerAssertion {
    #[cfg(target_os = "macos")]
    id: macos::AssertionId,
}

impl PowerAssertion {
    /// Acquire a "prevent idle system sleep" assertion. `reason` is the
    /// human-readable name macOS shows in `pmset -g assertions`. Returns `None`
    /// if the OS refuses the assertion (we then simply behave as before — the
    /// link will still drop on sleep, but nothing else breaks).
    pub fn prevent_idle_sleep(reason: &str) -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            macos::create(reason).map(|id| PowerAssertion { id })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = reason;
            // No portable equivalent; treat as "held" so callers don't churn.
            Some(PowerAssertion {})
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for PowerAssertion {
    fn drop(&mut self) {
        macos::release(self.id);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::os::raw::{c_char, c_int, c_void};
    use tracing::{debug, warn};

    // Opaque CoreFoundation handle.
    type CFStringRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CFTypeRef = *const c_void;
    type CFStringEncoding = u32;

    // IOKit power-management types.
    type IOReturn = c_int;
    type IOPMAssertionId = u32;
    type IOPMAssertionLevel = u32;

    pub type AssertionId = IOPMAssertionId;

    const K_CF_STRING_ENCODING_UTF8: CFStringEncoding = 0x0800_0100;
    const K_IOPM_ASSERTION_LEVEL_ON: IOPMAssertionLevel = 255;
    const K_IO_RETURN_SUCCESS: IOReturn = 0;
    // kIOPMAssertionTypePreventUserIdleSystemSleep — blocks idle *system* sleep
    // but leaves display sleep alone, which is exactly what we want.
    const ASSERTION_TYPE: &str = "PreventUserIdleSystemSleep";

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            alloc: CFAllocatorRef,
            c_str: *const c_char,
            encoding: CFStringEncoding,
        ) -> CFStringRef;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            assertion_level: IOPMAssertionLevel,
            assertion_name: CFStringRef,
            assertion_id: *mut IOPMAssertionId,
        ) -> IOReturn;
        fn IOPMAssertionRelease(assertion_id: IOPMAssertionId) -> IOReturn;
    }

    /// Build a CFString from a Rust `&str` (NUL-terminated copy). Returns null on
    /// failure; the caller checks. The returned CFString is owned and must be
    /// `CFRelease`d.
    fn cfstring(s: &str) -> CFStringRef {
        let Ok(c) = std::ffi::CString::new(s) else {
            return std::ptr::null();
        };
        unsafe {
            CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        }
    }

    pub fn create(reason: &str) -> Option<AssertionId> {
        let type_str = cfstring(ASSERTION_TYPE);
        let name_str = cfstring(reason);
        if type_str.is_null() || name_str.is_null() {
            if !type_str.is_null() {
                unsafe { CFRelease(type_str) };
            }
            if !name_str.is_null() {
                unsafe { CFRelease(name_str) };
            }
            warn!("power: could not build assertion strings; sleep will drop the link");
            return None;
        }
        let mut id: IOPMAssertionId = 0;
        let rc = unsafe {
            IOPMAssertionCreateWithName(type_str, K_IOPM_ASSERTION_LEVEL_ON, name_str, &mut id)
        };
        // CoreFoundation copies the strings into the assertion; release our refs.
        unsafe {
            CFRelease(type_str);
            CFRelease(name_str);
        }
        if rc == K_IO_RETURN_SUCCESS {
            debug!("power: holding PreventUserIdleSystemSleep assertion (id {id})");
            Some(id)
        } else {
            warn!("power: IOPMAssertionCreateWithName failed (rc {rc}); sleep will drop the link");
            None
        }
    }

    pub fn release(id: AssertionId) {
        let rc = unsafe { IOPMAssertionRelease(id) };
        if rc == K_IO_RETURN_SUCCESS {
            debug!("power: released sleep assertion (id {id})");
        } else {
            warn!("power: IOPMAssertionRelease failed (rc {rc})");
        }
    }
}
