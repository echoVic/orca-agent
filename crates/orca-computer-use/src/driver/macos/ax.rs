//! Owned accessibility handles. Optional local metadata degrades to absence;
//! timeouts and required app/window/action reads retain typed errors.
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

#[derive(Clone, Copy)]
enum FailurePolicy {
    Required,
    Absent,
    Foreground,
}

fn attribute_result(
    result: AXError,
    name: &str,
    policy: FailurePolicy,
) -> Result<bool, DriverError> {
    if result == AXError::AttributeUnsupported || result == AXError::NoValue {
        return Ok(false);
    }
    if result == AXError::Failure && matches!(policy, FailurePolicy::Absent) {
        return Ok(false);
    }
    if result == AXError::Failure && matches!(policy, FailurePolicy::Foreground) {
        return Err(error(
            ErrorKind::NeedsForeground,
            format!("Accessibility {name} failed; cannot determine modality"),
        ));
    }
    check(result, name)?;
    Ok(true)
}

#[derive(Debug, PartialEq)]
pub(super) enum SubroleRead {
    Value(String),
    Absent,
    Failure,
}

impl SubroleRead {
    pub(super) fn as_deref(&self) -> Option<&str> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent | Self::Failure => None,
        }
    }
}

fn classify_subrole(result: AXError, value: Option<String>) -> Result<SubroleRead, DriverError> {
    match result {
        AXError::Failure => Ok(SubroleRead::Failure),
        AXError::NoValue | AXError::AttributeUnsupported => Ok(SubroleRead::Absent),
        _ => {
            check(result, "AXSubrole")?;
            Ok(value.map_or(SubroleRead::Absent, SubroleRead::Value))
        }
    }
}

pub(super) fn secure_text_role(role: &str, subrole: &SubroleRead) -> bool {
    role.is_empty()
        || subrole.as_deref() == Some("AXSecureTextField")
        || (matches!(subrole, SubroleRead::Failure)
            && matches!(role, "AXTextField" | "AXTextArea" | "AXComboBox"))
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
        self.attribute_with_policy(
            name,
            if optional {
                FailurePolicy::Absent
            } else {
                FailurePolicy::Required
            },
        )
    }

    fn attribute_with_policy(
        &self,
        name: &str,
        policy: FailurePolicy,
    ) -> Result<Option<CFRetained<CFType>>, DriverError> {
        let mut value = std::ptr::null();
        let result = unsafe {
            self.0
                .copy_attribute_value(&CFString::from_str(name), NonNull::from(&mut value))
        };
        if !attribute_result(result, name, policy)? {
            return Ok(None);
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

    // R7: optional descriptive metadata treats NoValue,
    // AttributeUnsupported and Failure as absent. Actions and app/window
    // validation retain required reads; subrole() preserves Failure for security.
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

    pub(super) fn modal_attribute_string(&self, name: &str) -> Result<Option<String>, DriverError> {
        self.attribute_with_policy(name, FailurePolicy::Foreground)?
            .map(|value| {
                value
                    .downcast_ref::<CFString>()
                    .map(|text| text.to_string())
                    .ok_or_else(|| {
                        error(
                            ErrorKind::NeedsForeground,
                            format!("Unreadable {name} metadata"),
                        )
                    })
            })
            .transpose()
    }

    pub(super) fn modal_attribute_bool(&self, name: &str) -> Result<Option<bool>, DriverError> {
        self.attribute_with_policy(name, FailurePolicy::Foreground)?
            .map(|value| {
                value
                    .downcast_ref::<CFBoolean>()
                    .map(|boolean| boolean.value())
                    .ok_or_else(|| {
                        error(
                            ErrorKind::NeedsForeground,
                            format!("Unreadable {name} metadata"),
                        )
                    })
            })
            .transpose()
    }

    pub(super) fn modal_attribute_element(&self, name: &str) -> Result<Option<Self>, DriverError> {
        self.attribute_with_policy(name, FailurePolicy::Foreground)?
            .map(|value| {
                let element = value.downcast::<AXUIElement>().map_err(|_| {
                    error(
                        ErrorKind::NeedsForeground,
                        format!("Unreadable {name} metadata"),
                    )
                })?;
                Self::from_owned(element)
            })
            .transpose()
    }

    /// Security must distinguish a failed read from a field with no subrole.
    pub(super) fn subrole(&self) -> Result<SubroleRead, DriverError> {
        let mut value = std::ptr::null();
        let result = unsafe {
            self.0
                .copy_attribute_value(&CFString::from_str("AXSubrole"), NonNull::from(&mut value))
        };
        let value =
            NonNull::new(value.cast_mut()).map(|value| unsafe { CFRetained::from_raw(value) });
        classify_subrole(
            result,
            value.and_then(|value| {
                value
                    .downcast_ref::<CFString>()
                    .map(|text| text.to_string())
            }),
        )
    }

    pub(super) fn is_secure(&self) -> Result<bool, DriverError> {
        let Some(role) = self.optional_attribute_string("AXRole")? else {
            return Ok(true); // An unclassifiable keyboard recipient fails closed.
        };
        let subrole = self.subrole()?;
        Ok(secure_text_role(&role, &subrole))
    }

    pub(super) fn optional_attribute_element(
        &self,
        name: &str,
    ) -> Result<Option<Self>, DriverError> {
        self.attribute_with_optional_absence(name, true)?
            .and_then(|v| v.downcast::<AXUIElement>().ok())
            .map(Self::from_owned)
            .transpose()
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
        let Some(value) = self.attribute_with_optional_absence(name, deadline.is_some())? else {
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
            .filter_map(|element| match Self::from_owned(element) {
                Err(failure) if deadline.is_some() && failure.kind == ErrorKind::Stale => None,
                result => Some(result),
            })
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
            let Some(value) = self.attribute_with_optional_absence(name, deadline.is_some())?
            else {
                if deadline.is_some() {
                    return Ok(None);
                }
                return Err(error(
                    ErrorKind::Failed,
                    format!("Accessibility {name} is missing"),
                ));
            };
            let Some(value) = value.downcast_ref::<AXValue>() else {
                if deadline.is_some() {
                    return Ok(None);
                }
                return Err(error(
                    ErrorKind::Failed,
                    format!("Accessibility {name} has an invalid type"),
                ));
            };
            if !unsafe { value.value(kind, pointer) } {
                if deadline.is_some() {
                    return Ok(None);
                }
                return Err(error(ErrorKind::Failed, format!("Could not decode {name}")));
            }
        }
        Ok(Some([point.x, point.y, size.width, size.height]))
    }

    /// AXFocused can select all text. Collapse that selection when the
    /// optional range metadata is available, without blocking text delivery.
    pub(super) fn collapse_selection_to_end(&self) {
        use objc2_core_foundation::CFRange;
        let Some(location) = self
            .attribute("AXNumberOfCharacters")
            .ok()
            .flatten()
            .and_then(|value| {
                value
                    .downcast_ref::<CFNumber>()
                    .and_then(CFNumber::as_isize)
            })
            .filter(|length| *length >= 0)
        else {
            return;
        };
        let mut range = CFRange {
            location,
            length: 0,
        };
        if let Some(value) =
            unsafe { AXValue::new(AXValueType::CFRange, NonNull::from(&mut range).cast()) }
        {
            let _ = unsafe {
                self.0
                    .set_attribute_value(&CFString::from_str("AXSelectedTextRange"), value.as_ref())
            };
        }
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
    fn part4_modal_native_errors_preserve_failure_stale_and_timeout() {
        assert_eq!(
            attribute_result(AXError::Failure, "AXModal", FailurePolicy::Foreground)
                .unwrap_err()
                .kind,
            ErrorKind::NeedsForeground
        );
        assert_eq!(
            attribute_result(
                AXError::InvalidUIElement,
                "AXModal",
                FailurePolicy::Foreground
            )
            .unwrap_err()
            .kind,
            ErrorKind::Stale
        );
        assert_eq!(
            attribute_result(
                AXError::CannotComplete,
                "AXModal",
                FailurePolicy::Foreground
            )
            .unwrap_err()
            .kind,
            ErrorKind::Failed
        );
        for result in [AXError::NoValue, AXError::AttributeUnsupported] {
            assert!(!attribute_result(result, "AXModal", FailurePolicy::Foreground).unwrap());
        }
        assert!(attribute_result(AXError::Success, "AXModal", FailurePolicy::Foreground).unwrap());
    }

    #[test]
    fn unreadable_text_roles_are_secure_but_absent_subroles_are_ordinary() {
        for role in ["AXTextField", "AXTextArea", "AXComboBox"] {
            for result in [
                AXError::NoValue,
                AXError::AttributeUnsupported,
                AXError::Success,
            ] {
                let subrole = classify_subrole(result, None).unwrap();
                assert_eq!(subrole, SubroleRead::Absent);
                assert!(!secure_text_role(role, &subrole), "{role}: {result:?}");
            }
            let failed = classify_subrole(AXError::Failure, None).unwrap();
            assert_eq!(failed, SubroleRead::Failure);
            assert!(secure_text_role(role, &failed));
            assert!(!secure_text_role(
                role,
                &SubroleRead::Value("AXUnknown".into())
            ));
            assert!(secure_text_role(
                role,
                &SubroleRead::Value("AXSecureTextField".into())
            ));
        }
        for role in ["AXButton", "AXScrollArea", "AXRow", "AXWindow"] {
            assert!(!secure_text_role(role, &SubroleRead::Failure));
            assert!(!secure_text_role(role, &SubroleRead::Absent));
        }
        assert!(secure_text_role("", &SubroleRead::Absent));
        assert!(classify_subrole(AXError::CannotComplete, None).is_err());
        assert_eq!(
            classify_subrole(AXError::InvalidUIElement, None)
                .unwrap_err()
                .kind,
            ErrorKind::Stale
        );
    }

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
