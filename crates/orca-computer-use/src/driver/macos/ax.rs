//! Owned accessibility handles. Unsupported optional attributes are absent;
//! native failures (including invalid handles) remain typed driver errors.
use std::ptr::NonNull;
use std::time::Instant;

use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFNumber, CFRetained, CFString, CFType, CGPoint, CGSize,
};

use super::error;
use crate::protocol::{DriverError, ErrorKind};

#[derive(Clone)]
pub(crate) struct AxElement(pub(crate) CFRetained<AXUIElement>);

// AX handles identify remote UI objects and have no AppKit thread affinity.
// Ownership may move with the serialized driver (the in-process harness does
// this once at launch). Do not implement Sync: requests use them on one thread.
unsafe impl Send for AxElement {}

fn check(result: AXError, operation: &str) -> Result<(), DriverError> {
    if result == AXError::Success {
        return Ok(());
    }
    if result == AXError::InvalidUIElement {
        return Err(super::actions::stale());
    }
    Err(error(
        ErrorKind::Failed,
        format!("Accessibility {operation} failed: {result:?}"),
    ))
}

impl AxElement {
    pub(super) fn system_wide() -> Result<Self, DriverError> {
        Self::from_owned(unsafe { AXUIElement::new_system_wide() })
    }
    pub(super) fn pid(&self) -> Result<i32, DriverError> {
        let mut pid = 0;
        check(unsafe { self.0.pid(NonNull::from(&mut pid)) }, "read pid")?;
        Ok(pid)
    }
    pub(crate) fn application(pid: i32) -> Result<Self, DriverError> {
        Self::from_owned(unsafe { AXUIElement::new_application(pid) })
    }

    pub(super) fn from_owned(element: CFRetained<AXUIElement>) -> Result<Self, DriverError> {
        check(
            unsafe { element.set_messaging_timeout(2.0) },
            "set message timeout",
        )?;
        Ok(Self(element))
    }

    fn attribute(&self, name: &str) -> Result<Option<CFRetained<CFType>>, DriverError> {
        self.attribute_with_optional_absence(name, false)
    }

    fn attribute_with_optional_absence(
        &self,
        name: &str,
        optional: bool,
    ) -> Result<Option<CFRetained<CFType>>, DriverError> {
        let mut value = std::ptr::null();
        let result = unsafe {
            self.0
                .copy_attribute_value(&CFString::from_str(name), NonNull::from(&mut value))
        };
        if result == AXError::AttributeUnsupported || result == AXError::NoValue {
            return Ok(None);
        }
        match check(result, name) {
            Err(_) if optional && result == AXError::Failure => return Ok(None),
            result => result?,
        }
        Ok(NonNull::new(value.cast_mut()).map(|value| unsafe { CFRetained::from_raw(value) }))
    }

    pub(crate) fn attribute_string(&self, name: &str) -> Result<Option<String>, DriverError> {
        Ok(self.attribute(name)?.and_then(|value| {
            if let Some(text) = value.downcast_ref::<CFString>() {
                Some(text.to_string())
            } else {
                value
                    .downcast_ref::<CFNumber>()
                    .and_then(|number| number.as_f64())
                    .map(|number| number.to_string())
            }
        }))
    }

    // R7: optional title/description/label/placeholder/value reads
    // treat NoValue, AttributeUnsupported and Failure as absent. Required
    // role/position/size/window/children reads and actions must not use this.
    pub(super) fn optional_attribute_string(
        &self,
        name: &str,
    ) -> Result<Option<String>, DriverError> {
        Ok(self
            .attribute_with_optional_absence(name, true)?
            .and_then(|value| {
                if let Some(text) = value.downcast_ref::<CFString>() {
                    Some(text.to_string())
                } else {
                    value
                        .downcast_ref::<CFNumber>()
                        .and_then(|n| n.as_f64())
                        .map(|n| n.to_string())
                }
            }))
    }

    /// Observations and stale checks must use the same optional label reads.
    /// The traversal budget is checked before every native attribute call.
    pub(super) fn label(&self, deadline: Option<Instant>) -> Result<Option<String>, DriverError> {
        for name in ["AXTitle", "AXDescription", "AXPlaceholderValue"] {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
            if let Some(label) = self
                .optional_attribute_string(name)?
                .filter(|s| !s.is_empty())
            {
                return Ok(Some(label));
            }
        }
        Ok(None)
    }

    #[allow(dead_code)]
    pub(crate) fn attribute_bool(&self, name: &str) -> Result<Option<bool>, DriverError> {
        Ok(self
            .attribute(name)?
            .and_then(|v| v.downcast_ref::<CFBoolean>().map(|v| v.value())))
    }

    pub(crate) fn attribute_element(&self, name: &str) -> Result<Option<Self>, DriverError> {
        self.attribute(name)?
            .and_then(|v| v.downcast::<AXUIElement>().ok())
            .map(Self::from_owned)
            .transpose()
    }

    pub(crate) fn elements(&self, name: &str) -> Result<Vec<Self>, DriverError> {
        self.elements_with_deadline(name, None)
    }

    fn elements_with_deadline(
        &self,
        name: &str,
        deadline: Option<Instant>,
    ) -> Result<Vec<Self>, DriverError> {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(vec![]);
        }
        let Some(value) = self.attribute(name)? else {
            return Ok(vec![]);
        };
        let Some(array) = value.downcast_ref::<CFArray>() else {
            return Ok(vec![]);
        };
        (0..array.len())
            .take_while(|_| deadline.is_none_or(|d| Instant::now() < d))
            .filter_map(|i| {
                // AX arrays own Core Foundation objects; validate each dynamic type.
                let value = unsafe { &*array.value_at_index(i as isize).cast::<CFType>() };
                value
                    .downcast_ref::<AXUIElement>()
                    .map(|v| unsafe { CFRetained::retain(NonNull::from(v)) })
            })
            .map(Self::from_owned)
            .collect()
    }

    #[allow(dead_code)]
    pub(crate) fn children(&self) -> Result<Vec<Self>, DriverError> {
        self.elements("AXChildren")
    }

    pub(super) fn children_until(&self, deadline: Instant) -> Result<Vec<Self>, DriverError> {
        self.elements_with_deadline("AXChildren", Some(deadline))
    }

    pub(crate) fn position_size(&self) -> Result<[f64; 4], DriverError> {
        self.position_size_with_deadline(None)?
            .ok_or_else(|| error(ErrorKind::Failed, "Accessibility position/size is missing"))
    }

    pub(super) fn position_size_until(
        &self,
        deadline: Instant,
    ) -> Result<Option<[f64; 4]>, DriverError> {
        self.position_size_with_deadline(Some(deadline))
    }

    fn position_size_with_deadline(
        &self,
        deadline: Option<Instant>,
    ) -> Result<Option<[f64; 4]>, DriverError> {
        let mut point = CGPoint::default();
        let mut size = CGSize::default();
        for (name, kind, pointer) in [
            (
                "AXPosition",
                AXValueType::CGPoint,
                NonNull::from(&mut point).cast(),
            ),
            (
                "AXSize",
                AXValueType::CGSize,
                NonNull::from(&mut size).cast(),
            ),
        ] {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(None);
            }
            let value = self.attribute(name)?.ok_or_else(|| {
                error(
                    ErrorKind::Failed,
                    format!("Accessibility {name} is missing"),
                )
            })?;
            let value = value.downcast_ref::<AXValue>().ok_or_else(|| {
                error(
                    ErrorKind::Failed,
                    format!("Accessibility {name} has an invalid type"),
                )
            })?;
            if !unsafe { value.value(kind, pointer) } {
                return Err(error(ErrorKind::Failed, format!("Could not decode {name}")));
            }
        }
        Ok(Some([point.x, point.y, size.width, size.height]))
    }

    // These native operations are the Task 7 action interface.
    #[allow(dead_code)]
    pub(crate) fn actions(&self) -> Result<Vec<String>, DriverError> {
        let mut array = std::ptr::null();
        check(
            unsafe { self.0.copy_action_names(NonNull::from(&mut array)) },
            "list actions",
        )?;
        let array = NonNull::new(array.cast_mut())
            .ok_or_else(|| error(ErrorKind::Failed, "Accessibility actions returned no array"))?;
        let array = unsafe { CFRetained::from_raw(array) };
        Ok((0..array.len())
            .filter_map(|i| {
                let value = unsafe { &*array.value_at_index(i as isize).cast::<CFType>() };
                value.downcast_ref::<CFString>().map(ToString::to_string)
            })
            .collect())
    }
    #[allow(dead_code)]
    pub(crate) fn perform(&self, action: &str) -> Result<(), DriverError> {
        check(
            unsafe { self.0.perform_action(&CFString::from_str(action)) },
            action,
        )
    }
    pub(crate) fn set_bool(&self, name: &str, value: bool) -> Result<(), DriverError> {
        check(
            unsafe {
                self.0
                    .set_attribute_value(&CFString::from_str(name), CFBoolean::new(value).as_ref())
            },
            name,
        )
    }
    pub(super) fn set_number(&self, name: &str, value: f64) -> Result<(), DriverError> {
        check(
            unsafe {
                self.0.set_attribute_value(
                    &CFString::from_str(name),
                    CFNumber::new_f64(value).as_ref(),
                )
            },
            name,
        )
    }

    #[allow(dead_code)]
    pub(crate) fn set_string(&self, name: &str, value: &str) -> Result<(), DriverError> {
        check(
            unsafe {
                self.0.set_attribute_value(
                    &CFString::from_str(name),
                    CFString::from_str(value).as_ref(),
                )
            },
            name,
        )
    }
    #[allow(dead_code)]
    pub(crate) fn element_at_position(&self, x: f32, y: f32) -> Result<Self, DriverError> {
        let mut element = std::ptr::null();
        check(
            unsafe {
                self.0
                    .copy_element_at_position(x, y, NonNull::from(&mut element))
            },
            "hit test",
        )?;
        let element = NonNull::new(element.cast_mut()).ok_or_else(|| {
            error(
                ErrorKind::Failed,
                "Accessibility hit test returned no element",
            )
        })?;
        Self::from_owned(unsafe { CFRetained::from_raw(element) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_elements_require_a_fresh_observation() {
        let failure = check(AXError::InvalidUIElement, "AXRole").unwrap_err();
        assert_eq!(failure.kind, ErrorKind::Stale);
        assert_eq!(
            failure.message,
            "the window changed since the last observe; observe again"
        );
        assert_eq!(
            check(AXError::Failure, "AXRole").unwrap_err().kind,
            ErrorKind::Failed
        );
        assert!(check(AXError::Success, "AXRole").is_ok());
    }
}
