use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_app_kit::NSRunningApplication;
use objc2_application_services::AXIsProcessTrusted;
use objc2_core_graphics::{
    CGEvent, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy,
    CGEventType, CGPreflightScreenCaptureAccess,
};

use crate::protocol::Permissions;

// The probe never changes or suppresses an event, and has no retained context.
unsafe extern "C-unwind" fn passthrough(
    _proxy: CGEventTapProxy,
    _kind: CGEventType,
    event: NonNull<CGEvent>,
    _context: *mut c_void,
) -> *mut CGEvent {
    event.as_ptr()
}

fn probe_parameters() -> (
    CGEventTapLocation,
    CGEventTapPlacement,
    CGEventTapOptions,
    u64,
) {
    super::esc_tap::parameters()
}

pub(super) fn check() -> Permissions {
    let (location, placement, options, mask) = probe_parameters();
    let tap = unsafe {
        CGEvent::tap_create(
            location,
            placement,
            options,
            mask,
            Some(passthrough),
            std::ptr::null_mut(),
        )
    };
    let event_tap = tap.is_some();
    if let Some(tap) = tap {
        tap.invalidate();
    }
    Permissions {
        accessibility: unsafe { AXIsProcessTrusted() },
        screen_recording: CGPreflightScreenCaptureAccess(),
        event_tap,
    }
}

fn detect_host(
    mut environment: impl FnMut(&str) -> Option<String>,
    parent_host: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if environment("TMUX").is_some() || environment("STY").is_some() {
        return None;
    }
    environment("__CFBundleIdentifier").or_else(parent_host)
}
pub(super) fn host_bundle_id() -> Option<String> {
    detect_host(
        |name| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned()),
        parent_host_bundle_id,
    )
}
fn parent_host_bundle_id() -> Option<String> {
    let mut pid = std::process::id() as i32;
    while let Some(parent) = parent_pid(pid) {
        if parent <= 0 || parent == pid {
            break;
        }
        if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(parent)
            && let Some(bundle_id) = app.bundleIdentifier()
        {
            return Some(bundle_id.to_string());
        }
        pid = parent;
    }
    None
}

fn parent_pid(pid: i32) -> Option<i32> {
    // Darwin's 64-bit public sysctl ABI: sys/sysctl.h kinfo_proc is 648 bytes,
    // kp_eproc.e_ppid is at offset 560 (both arm64 and x86_64). libc exposes the
    // sysctl constants but not this struct. Check the returned size before reading.
    const KINFO_PROC_SIZE: usize = 648;
    const PPID_OFFSET: usize = 560;
    let mut record = [0_u8; KINFO_PROC_SIZE];
    let mut size = record.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            record.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size != KINFO_PROC_SIZE {
        return None;
    }
    Some(i32::from_ne_bytes(
        record[PPID_OFFSET..PPID_OFFSET + 4].try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_probe_matches_the_listener_tap() {
        assert_eq!(probe_parameters(), super::super::esc_tap::parameters());
    }
    #[test]
    fn multiplexers_never_trust_an_inherited_or_parent_host() {
        for (tmux, screen, expected) in [
            (false, false, Some("host")),
            (true, false, None),
            (false, true, None),
        ] {
            let host = detect_host(
                |key| match key {
                    "__CFBundleIdentifier" => Some("host".into()),
                    "TMUX" if tmux => Some("/tmp/test-tmux".into()),
                    "STY" if screen => Some("screen-session".into()),
                    _ => None,
                },
                || Some("parent".into()),
            );
            assert_eq!(host.as_deref(), expected);
        }
        assert_eq!(
            detect_host(
                |key| (key == "TMUX").then(String::new),
                || Some("parent".into())
            ),
            None
        );
        assert_eq!(
            detect_host(|_| None, || Some("parent".into())),
            Some("parent".into())
        );
    }
}
