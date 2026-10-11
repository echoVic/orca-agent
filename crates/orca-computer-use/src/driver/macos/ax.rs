//! Owned accessibility handles. Unsupported optional attributes are absent;
//! native failures (including invalid handles) remain typed driver errors.
use std::ptr::NonNull;

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
    Err(error(
        if result == AXError::InvalidUIElement {
            ErrorKind::Stale
        } else {
            ErrorKind::Failed
        },
        format!("Accessibility {operation} failed: {result:?}"),
    ))
}

impl AxElement {
    pub(crate) fn application(pid: i32) -> Result<Self, DriverError> {
        Self::from_owned(unsafe { AXUIElement::new_application(pid) })
    }

    fn from_owned(element: CFRetained<AXUIElement>) -> Result<Self, DriverError> {
        check(
            unsafe { element.set_messaging_timeout(2.0) },
            "set message timeout",
        )?;
        Ok(Self(element))
    }

    fn attribute(&self, name: &str) -> Result<Option<CFRetained<CFType>>, DriverError> {
        self.attribute_with_empty_label(name, false)
    }

    fn attribute_with_empty_label(
        &self,
        name: &str,
        empty_label: bool,
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
            // AppKit's unlabeled NSTextView advertises AXDescription but
            // returns kAXErrorFailure. Only this optional label failure is
            // absent; timeouts and stale handles always propagate.
            Err(_) if empty_label && result == AXError::Failure => return Ok(None),
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

    pub(super) fn label_attribute(&self, name: &str) -> Result<Option<String>, DriverError> {
        Ok(self
            .attribute_with_empty_label(name, true)?
            .and_then(|value| {
                value
                    .downcast_ref::<CFString>()
                    .map(ToString::to_string)
                    .filter(|s| !s.is_empty())
            }))
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
        let Some(value) = self.attribute(name)? else {
            return Ok(vec![]);
        };
        let Some(array) = value.downcast_ref::<CFArray>() else {
            return Ok(vec![]);
        };
        (0..array.len())
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

    pub(crate) fn children(&self) -> Result<Vec<Self>, DriverError> {
        self.elements("AXChildren")
    }

    pub(crate) fn position_size(&self) -> Result<[f64; 4], DriverError> {
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
        Ok([point.x, point.y, size.width, size.height])
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
