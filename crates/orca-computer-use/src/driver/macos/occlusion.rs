//! Window-stack backstop for known remote-control overlays.

use std::ptr::NonNull;

use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint};
use objc2_core_graphics::{
    CGDisplayBounds, CGError, CGGetDisplaysWithPoint, CGWindowListCopyWindowInfo,
    CGWindowListOption, kCGNullWindowID, kCGWindowAlpha, kCGWindowBounds, kCGWindowLayer,
    kCGWindowOwnerName, kCGWindowOwnerPID,
};

struct RemoteOverlayOwner {
    name: &'static str,
    signing_identifier: &'static str,
    team: &'static str,
}

const REMOTE_OVERLAY_OWNERS: &[RemoteOverlayOwner] = &[RemoteOverlayOwner {
    name: "UURemoteServer",
    signing_identifier: "com.netease.uuremote.server",
    team: "PU9BNSBJW7",
}];

// SecCode and SecRequirement are owned CF objects, released through CFRetained.
#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecGuestAttributePid: &'static CFString;
    fn SecRequirementCreateWithString(
        text: *const CFString,
        flags: u32,
        requirement: *mut *const CFType,
    ) -> i32;
    fn SecCodeCopyGuestWithAttributes(
        host: *const CFType,
        attributes: *const CFDictionary,
        flags: u32,
        guest: *mut *const CFType,
    ) -> i32;
    fn SecCodeCheckValidity(code: *const CFType, flags: u32, requirement: *const CFType) -> i32;
}

fn overlay_requirement(owner: &RemoteOverlayOwner) -> Option<CFRetained<CFType>> {
    let text = CFString::from_str(&format!(
        r#"anchor apple generic and identifier "{}" and certificate leaf[subject.OU] = "{}""#,
        owner.signing_identifier, owner.team,
    ));
    let mut requirement = std::ptr::null();
    if unsafe { SecRequirementCreateWithString(&*text, 0, &mut requirement) } != 0 {
        return None;
    }
    NonNull::new(requirement.cast_mut()).map(|value| unsafe { CFRetained::from_raw(value) })
}

fn authenticated_overlay(pid: i32, owner: &RemoteOverlayOwner) -> bool {
    let Some(requirement) = overlay_requirement(owner) else {
        return false;
    };
    let attributes = CFDictionary::<CFString, CFNumber>::from_slices(
        &[unsafe { kSecGuestAttributePid }],
        &[CFNumber::new_i32(pid).as_ref()],
    );
    let mut code = std::ptr::null();
    if unsafe {
        SecCodeCopyGuestWithAttributes(std::ptr::null(), attributes.as_opaque(), 0, &mut code)
    } != 0
    {
        return false;
    }
    let Some(code) = NonNull::new(code.cast_mut()) else {
        return false;
    };
    let code = unsafe { CFRetained::from_raw(code) };
    unsafe { SecCodeCheckValidity(&*code, 0, &*requirement) == 0 }
}

const MAX_WINDOWS: usize = 4096;

#[derive(Clone, Debug)]
struct WindowRecord {
    owner_pid: i32,
    owner_name: String,
    layer: i32,
    bounds: [f64; 4],
    alpha: f64,
}

fn point_is_uncovered(
    windows: &[WindowRecord],
    point: [f64; 2],
    display_bounds: [f64; 4],
    target_pid: i32,
    hit_pid: i32,
    authenticated: impl Fn(i32, &RemoteOverlayOwner) -> bool,
) -> bool {
    if hit_pid == target_pid {
        return true;
    }
    if !contains(display_bounds, point)
        || windows
            .iter()
            .any(|window| !valid_bounds(window.bounds) || !(0.0..=1.0).contains(&window.alpha))
    {
        return false;
    }
    let is_overlay = |window: &WindowRecord| {
        window.layer >= 1000
            && contains(window.bounds, point)
            && window.bounds[0] <= display_bounds[0]
            && window.bounds[1] <= display_bounds[1]
            && window.bounds[0] + window.bounds[2] >= display_bounds[0] + display_bounds[2]
            && window.bounds[1] + window.bounds[3] >= display_bounds[1] + display_bounds[3]
            && REMOTE_OVERLAY_OWNERS.iter().any(|owner| {
                owner.name == window.owner_name && authenticated(window.owner_pid, owner)
            })
    };
    windows
        .iter()
        .any(|window| window.owner_pid == hit_pid && is_overlay(window))
        && windows
            .iter()
            .find(|window| {
                window.alpha != 0.0 && contains(window.bounds, point) && !is_overlay(window)
            })
            .is_some_and(|window| window.owner_pid == target_pid)
}

fn valid_bounds([x, y, width, height]: [f64; 4]) -> bool {
    [x, y, width, height, x + width, y + height]
        .iter()
        .all(|value| value.is_finite())
        && width >= 0.0
        && height >= 0.0
}

fn contains(bounds: [f64; 4], [x, y]: [f64; 2]) -> bool {
    valid_bounds(bounds)
        && x >= bounds[0]
        && y >= bounds[1]
        && x < bounds[0] + bounds[2]
        && y < bounds[1] + bounds[3]
}

fn dictionary_value<'a>(dictionary: &'a CFDictionary, key: &CFString) -> Option<&'a CFType> {
    // CGWindowListCopyWindowInfo dictionaries retain their CF values.
    unsafe {
        dictionary
            .value((key as *const CFString).cast())
            .cast::<CFType>()
            .as_ref()
    }
}

fn window_record(dictionary: &CFDictionary) -> Option<WindowRecord> {
    let number = |key| dictionary_value(dictionary, key)?.downcast_ref::<CFNumber>();
    let bounds =
        dictionary_value(dictionary, unsafe { kCGWindowBounds })?.downcast_ref::<CFDictionary>()?;
    let coordinate = |key| {
        dictionary_value(bounds, &CFString::from_str(key))?
            .downcast_ref::<CFNumber>()?
            .as_f64()
    };
    // Owner names are optional; missing or oversized names cannot qualify.
    let owner_name = dictionary_value(dictionary, unsafe { kCGWindowOwnerName })
        .and_then(|value| value.downcast_ref::<CFString>())
        .filter(|name| name.length() <= 128)
        .map(|name| name.to_string())
        .unwrap_or_default();
    Some(WindowRecord {
        owner_pid: number(unsafe { kCGWindowOwnerPID })?.as_i32()?,
        owner_name,
        layer: number(unsafe { kCGWindowLayer })?.as_i32()?,
        bounds: [
            coordinate("X")?,
            coordinate("Y")?,
            coordinate("Width")?,
            coordinate("Height")?,
        ],
        alpha: number(unsafe { kCGWindowAlpha })?.as_f64()?,
    })
}

pub(super) fn target_under_remote_overlay(
    target_pid: i32,
    hit_pid: i32,
    point: CGPoint,
) -> Option<bool> {
    let mut display = 0;
    let mut count = 0;
    if unsafe { CGGetDisplaysWithPoint(point, 1, &mut display, &mut count) } != CGError::Success
        || count != 1
    {
        return None;
    }
    let display = CGDisplayBounds(display);
    let windows =
        CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, kCGNullWindowID)?;
    // Never truncate a stack: a missing record could conceal a covering app.
    if windows.len() > MAX_WINDOWS {
        return None;
    }
    let mut records = Vec::with_capacity(windows.len());
    for index in 0..windows.len() {
        // The immutable, retained array contains CF dictionaries in front-to-back order.
        let value = unsafe {
            windows
                .value_at_index(index as isize)
                .cast::<CFType>()
                .as_ref()
        }?;
        records.push(window_record(value.downcast_ref::<CFDictionary>()?)?);
    }
    Some(point_is_uncovered(
        &records,
        [point.x, point.y],
        [
            display.origin.x,
            display.origin.y,
            display.size.width,
            display.size.height,
        ],
        target_pid,
        hit_pid,
        authenticated_overlay,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: [f64; 4] = [0.0, 0.0, 1728.0, 1117.0];
    const POINT: [f64; 2] = [200.0, 200.0];
    const TARGET: i32 = 10;
    const UU: i32 = 20;
    const OTHER: i32 = 30;

    fn window(owner_pid: i32) -> WindowRecord {
        WindowRecord {
            owner_pid,
            owner_name: "App".into(),
            layer: 0,
            bounds: [100.0, 100.0, 800.0, 600.0],
            alpha: 1.0,
        }
    }

    fn overlay() -> WindowRecord {
        WindowRecord {
            owner_name: "UURemoteServer".into(),
            layer: 1000,
            bounds: DISPLAY,
            ..window(UU)
        }
    }

    fn uncovered(windows: &[WindowRecord], hit_pid: i32) -> bool {
        point_is_uncovered(windows, POINT, DISPLAY, TARGET, hit_pid, |_, _| true)
    }

    #[test]
    fn an_unauthenticated_owner_with_the_uu_name_and_geometry_is_refused() {
        assert!(!point_is_uncovered(
            &[overlay(), window(TARGET)],
            POINT,
            DISPLAY,
            TARGET,
            UU,
            |_, _| false,
        ));
    }

    #[test]
    fn the_uu_requirement_parses() {
        for owner in REMOTE_OVERLAY_OWNERS {
            assert!(overlay_requirement(owner).is_some());
        }
    }

    #[test]
    fn the_test_process_does_not_authenticate_as_the_uu_helper() {
        for owner in REMOTE_OVERLAY_OWNERS {
            assert!(!authenticated_overlay(std::process::id() as i32, owner));
        }
    }

    #[test]
    fn authentication_only_runs_after_the_owner_and_geometry_match() {
        let mut wrong_name = overlay();
        wrong_name.owner_name = "OtherRemoteServer".into();
        let mut wrong_geometry = overlay();
        wrong_geometry.bounds = [0.0, 0.0, 100.0, 100.0];
        for window in [wrong_name, wrong_geometry] {
            assert!(!point_is_uncovered(
                &[window, self::window(TARGET)],
                POINT,
                DISPLAY,
                TARGET,
                UU,
                |_, _| panic!("non-overlay windows must not trigger signature checks"),
            ));
        }
        assert!(point_is_uncovered(
            &[],
            POINT,
            DISPLAY,
            TARGET,
            TARGET,
            |_, _| panic!("matching AX hits must not trigger signature checks")
        ));
    }

    #[test]
    fn a_uu_overlay_over_the_target_is_allowed() {
        assert!(uncovered(&[overlay(), window(TARGET)], UU));
    }

    #[test]
    fn a_uu_overlay_over_another_app_over_the_target_is_refused() {
        assert!(!uncovered(&[overlay(), window(OTHER), window(TARGET)], UU));
    }

    #[test]
    fn an_unknown_owner_with_the_same_overlay_shape_is_refused() {
        for name in [
            "OtherRemoteServer",
            "uuremoteserver",
            "UURemoteServer Helper",
        ] {
            let mut unknown = overlay();
            unknown.owner_name = name.into();
            assert!(!uncovered(&[unknown, window(TARGET)], UU));
        }
    }

    #[test]
    fn a_partial_screen_or_lower_layer_uu_window_is_refused() {
        for (bounds, layer) in [
            ([0.0, 0.0, 1727.0, 1117.0], 1000),
            ([0.0, 1.0, 1728.0, 1116.0], 1000),
            (DISPLAY, 999),
        ] {
            let mut uu = overlay();
            uu.bounds = bounds;
            uu.layer = layer;
            assert!(!uncovered(&[uu, window(TARGET)], UU));
        }
    }

    #[test]
    fn another_app_hit_without_an_overlay_is_refused() {
        assert!(!uncovered(&[window(OTHER), window(TARGET)], OTHER));
        assert!(!uncovered(&[window(TARGET)], OTHER));
    }

    #[test]
    fn a_target_hit_is_allowed_without_a_window_list() {
        assert!(uncovered(&[], TARGET));
        assert!(uncovered(&[window(OTHER)], TARGET));
    }

    #[test]
    fn the_overlay_must_belong_to_the_hit_process() {
        assert!(!uncovered(&[overlay(), window(TARGET)], OTHER));
    }

    #[test]
    fn transparent_and_off_point_windows_are_skipped_in_stack_order() {
        let mut transparent = window(OTHER);
        transparent.alpha = 0.0;
        let mut off_point = window(OTHER);
        off_point.bounds = [900.0, 100.0, 200.0, 200.0];
        assert!(uncovered(
            &[transparent, overlay(), off_point, window(TARGET)],
            UU,
        ));
    }

    #[test]
    fn a_nonzero_alpha_cover_is_not_skipped() {
        let mut cover = window(OTHER);
        cover.alpha = 0.001;
        assert!(!uncovered(&[overlay(), cover, window(TARGET)], UU));
    }

    #[test]
    fn another_uu_window_is_not_skipped_just_because_its_owner_has_an_overlay() {
        let mut cover = window(UU);
        cover.owner_name = "UURemoteServer".into();
        cover.layer = 1000;
        assert!(!uncovered(&[overlay(), cover, window(TARGET)], UU));
    }

    #[test]
    fn an_overlay_without_an_underlying_target_is_refused() {
        assert!(!uncovered(&[overlay()], UU));
        assert!(!uncovered(&[], UU));
    }

    #[test]
    fn the_overlay_must_cover_the_display_containing_the_point() {
        let display = [-1920.0, 0.0, 1920.0, 1080.0];
        let point = [-200.0, 200.0];
        let mut uu = overlay();
        uu.bounds = display;
        uu.layer = 1001;
        let mut target = window(TARGET);
        target.bounds = [-800.0, 100.0, 800.0, 600.0];
        assert!(point_is_uncovered(
            &[uu, target.clone()],
            point,
            display,
            TARGET,
            UU,
            |_, _| true,
        ));
        assert!(!point_is_uncovered(
            &[overlay(), target],
            point,
            display,
            TARGET,
            UU,
            |_, _| true,
        ));
        assert!(!point_is_uncovered(
            &[overlay(), window(TARGET)],
            POINT,
            display,
            TARGET,
            UU,
            |_, _| true,
        ));
    }
}
