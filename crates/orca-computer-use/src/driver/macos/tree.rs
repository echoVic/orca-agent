use std::thread;
use std::time::{Duration, Instant};

use super::ax::AxElement;
use crate::{
    geometry::CaptureGeometry,
    observation::filter_elements,
    protocol::{DriverError, Element},
};

// The table is retained for Task 7's stale checks and native actions.
#[allow(dead_code)]
pub(crate) struct ElementTable {
    pub pid: i32,
    pub window_id: u32,
    pub window_frame: [f64; 4],
    pub geometry: CaptureGeometry,
    pub entries: Vec<TableEntry>,
}
#[allow(dead_code)]
pub(crate) struct TableEntry {
    pub element: AxElement,
    pub role: String,
    pub label: Option<String>,
    pub secure: bool,
}

const ROLES: &[&str] = &[
    "AXButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXPopUpButton",
    "AXMenuButton",
    "AXComboBox",
    "AXTextField",
    "AXTextArea",
    "AXSlider",
    "AXIncrementor",
    "AXLink",
    "AXMenuItem",
    "AXDisclosureTriangle",
    "AXColorWell",
    "AXScrollArea",
    "AXTabGroup",
    "AXCell",
    "AXRow",
];

fn role_words(role: &str) -> String {
    let mut result = String::new();
    for (i, ch) in role.strip_prefix("AX").unwrap_or(role).chars().enumerate() {
        if i > 0 && ch.is_uppercase() {
            result.push(' ');
        }
        result.extend(ch.to_lowercase());
    }
    result
}

fn walk(
    element: AxElement,
    depth: u16,
    geometry: CaptureGeometry,
    elements: &mut Vec<Element>,
    entries: &mut Vec<TableEntry>,
) -> Result<(), DriverError> {
    if depth > 30 {
        return Ok(());
    }
    let role = element.attribute_string("AXRole")?.unwrap_or_default();
    let retained_role = ROLES.contains(&role.as_str());
    let secure = element.attribute_string("AXSubrole")?.as_deref() == Some("AXSecureTextField");
    let description = if role == "AXStaticText" || role == "AXImage" {
        element.label_attribute("AXDescription")?
    } else {
        None
    };
    let mut label = None;
    // AXLabel is the description attribute on macOS; don't use localized role descriptions.
    for name in ["AXTitle", "AXDescription", "AXPlaceholderValue"]
        .into_iter()
        .filter(|_| retained_role || role == "AXStaticText" || role == "AXImage")
    {
        label = element.label_attribute(name)?;
        if label.is_some() {
            break;
        }
    }
    let value = if !secure && (retained_role || role == "AXStaticText" || role == "AXImage") {
        element.attribute_string("AXValue")?
    } else {
        None
    };
    if retained_role
        || ((role == "AXStaticText" || role == "AXImage")
            && (value.as_ref().is_some_and(|s| !s.is_empty()) || description.is_some()))
    {
        let [x, y, width, height] = geometry.to_image(element.position_size()?);
        // AX document frames include scrolled-off content. Report their
        // intersection with the captured image so coordinate targets are valid.
        let left = x.clamp(0, geometry.image_size.0 as i32);
        let top = y.clamp(0, geometry.image_size.1 as i32);
        let right = x
            .saturating_add(width)
            .clamp(left, geometry.image_size.0 as i32);
        let bottom = y
            .saturating_add(height)
            .clamp(top, geometry.image_size.1 as i32);
        let frame = [left, top, right - left, bottom - top];
        elements.push(Element {
            id: elements.len() as u32 + 1,
            depth,
            role: role_words(&role),
            label: label.clone(),
            value,
            secure,
            frame,
        });
        entries.push(TableEntry {
            element: element.clone(),
            role,
            label,
            secure,
        });
    }
    if depth < 30 {
        for child in element.children()? {
            walk(child, depth + 1, geometry, elements, entries)?;
        }
    }
    Ok(())
}

pub(super) fn collect(
    window: &AxElement,
    geometry: CaptureGeometry,
    query: Option<&str>,
    max_elements: u32,
) -> Result<(Vec<Element>, Vec<TableEntry>, u32, bool), DriverError> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let (mut elements, entries) = loop {
        let mut elements = vec![];
        let mut entries = vec![];
        walk(window.clone(), 0, geometry, &mut elements, &mut entries)?;
        if !elements.is_empty() || Instant::now() >= deadline {
            break (elements, entries);
        }
        thread::sleep(
            Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
        );
    };
    let no_element_tree = elements.is_empty();
    if let Some(query) = query {
        elements = filter_elements(&elements, query);
    }
    let omitted = elements
        .len()
        .saturating_sub(max_elements.min(1000) as usize) as u32;
    elements.truncate(max_elements.min(1000) as usize);
    let mut entries: Vec<_> = entries.into_iter().map(Some).collect();
    let mut selected = Vec::with_capacity(elements.len());
    for (index, element) in elements.iter_mut().enumerate() {
        selected.push(
            entries[element.id as usize - 1]
                .take()
                .expect("unique collected element ID"),
        );
        element.id = index as u32 + 1;
    }
    Ok((elements, selected, omitted, no_element_tree))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roles_are_stable_unlocalized_words() {
        assert_eq!(role_words("AXPopUpButton"), "pop up button");
        assert_eq!(role_words("AXTextField"), "text field");
        assert_eq!(role_words("AXRadioButton"), "radio button");
    }
}
