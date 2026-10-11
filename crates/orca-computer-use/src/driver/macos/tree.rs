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

// A narrow traversal seam shares the production recursion with synthetic trees.
trait TreeNode: Clone {
    type Entry;
    fn retained(
        &self,
        geometry: CaptureGeometry,
        deadline: Instant,
    ) -> Result<Option<(Element, Self::Entry)>, DriverError>;
    fn children_until(&self, deadline: Instant) -> Result<Vec<Self>, DriverError>;
}

impl TreeNode for AxElement {
    type Entry = TableEntry;
    fn retained(
        &self,
        geometry: CaptureGeometry,
        deadline: Instant,
    ) -> Result<Option<(Element, TableEntry)>, DriverError> {
        // Each expression below starts at most one AX read, except position/size,
        // whose helper checks the same deadline between its two native reads.
        macro_rules! read {
            ($operation:expr) => {{
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                $operation?
            }};
        }
        let role = read!(self.attribute_string("AXRole")).unwrap_or_default();
        let retained_role = ROLES.contains(&role.as_str());
        let secure =
            read!(self.attribute_string("AXSubrole")).as_deref() == Some("AXSecureTextField");
        let description = if role == "AXStaticText" || role == "AXImage" {
            read!(self.optional_attribute_string("AXDescription")).filter(|s| !s.is_empty())
        } else {
            None
        };
        let label = if retained_role || role == "AXStaticText" || role == "AXImage" {
            read!(self.label(Some(deadline)))
        } else {
            None
        };
        let value = if !secure && (retained_role || role == "AXStaticText" || role == "AXImage") {
            read!(self.optional_attribute_string("AXValue"))
        } else {
            None
        };
        if retained_role
            || ((role == "AXStaticText" || role == "AXImage")
                && (value.as_ref().is_some_and(|s| !s.is_empty()) || description.is_some()))
        {
            let Some(frame) = self.position_size_until(deadline)? else {
                return Ok(None);
            };
            let [x, y, width, height] = geometry.to_image(frame);
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
            let element = Element {
                id: 0,
                depth: 0,
                role: role_words(&role),
                label: label.clone(),
                value,
                secure,
                frame,
            };
            let entry = TableEntry {
                element: self.clone(),
                role,
                label,
                secure,
            };
            return Ok(Some((element, entry)));
        }
        Ok(None)
    }
    fn children_until(&self, deadline: Instant) -> Result<Vec<Self>, DriverError> {
        AxElement::children_until(self, deadline)
    }
}

#[derive(Clone, Copy, Default)]
struct Depth {
    ax: u16,
    retained: u16,
}

fn walk<N: TreeNode>(
    element: N,
    depth: Depth,
    geometry: CaptureGeometry,
    deadline: Instant,
    elements: &mut Vec<Element>,
    entries: &mut Vec<N::Entry>,
) -> Result<(), DriverError> {
    if depth.ax > 30 || Instant::now() >= deadline {
        return Ok(());
    }
    let mut child_depth = Depth {
        ax: depth.ax + 1,
        retained: depth.retained,
    };
    if let Some((mut retained, entry)) = element.retained(geometry, deadline)? {
        retained.id = elements.len() as u32 + 1;
        // Omitted structural nodes don't create ancestors in the output tree.
        retained.depth = depth.retained;
        child_depth.retained += 1;
        elements.push(retained);
        entries.push(entry);
    }
    if depth.ax < 30 && Instant::now() < deadline {
        for child in element.children_until(deadline)? {
            if Instant::now() >= deadline {
                break;
            }
            walk(child, child_depth, geometry, deadline, elements, entries)?;
        }
    }
    Ok(())
}

fn poll<E>(
    deadline: Instant,
    mut traversal: impl FnMut(Instant, &mut Vec<Element>, &mut Vec<E>) -> Result<(), DriverError>,
) -> Result<(Vec<Element>, Vec<E>), DriverError> {
    let mut elements = vec![];
    let mut entries = vec![];
    loop {
        // In particular, don't start a retry after the final capped sleep.
        if Instant::now() >= deadline {
            return Ok((elements, entries));
        }
        traversal(deadline, &mut elements, &mut entries)?;
        if !elements.is_empty() || Instant::now() >= deadline {
            return Ok((elements, entries));
        }
        thread::sleep(
            Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

pub(super) fn collect(
    window: &AxElement,
    geometry: CaptureGeometry,
    query: Option<&str>,
    max_elements: u32,
) -> Result<(Vec<Element>, Vec<TableEntry>, u32, bool), DriverError> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let (mut elements, entries) = poll(deadline, |deadline, elements, entries| {
        walk(
            window.clone(),
            Depth::default(),
            geometry,
            deadline,
            elements,
            entries,
        )
    })?;
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

#[cfg(test)]
mod regression_tests {
    use super::*;

    #[derive(Clone)]
    struct Node {
        label: Option<&'static str>,
        delay: Duration,
        children: Vec<Node>,
    }
    impl Node {
        fn group(children: Vec<Self>) -> Self {
            Self {
                label: None,
                delay: Duration::ZERO,
                children,
            }
        }
        fn control(label: &'static str) -> Self {
            Self {
                label: Some(label),
                delay: Duration::ZERO,
                children: vec![],
            }
        }
    }
    impl TreeNode for Node {
        type Entry = ();
        fn retained(
            &self,
            _geometry: CaptureGeometry,
            _deadline: Instant,
        ) -> Result<Option<(Element, ())>, DriverError> {
            thread::sleep(self.delay);
            Ok(self.label.map(|label| {
                (
                    Element {
                        id: 0,
                        depth: 0,
                        role: "control".into(),
                        label: Some(label.into()),
                        value: None,
                        secure: false,
                        frame: [0; 4],
                    },
                    (),
                )
            }))
        }
        fn children_until(&self, _deadline: Instant) -> Result<Vec<Self>, DriverError> {
            Ok(self.children.clone())
        }
    }
    fn geometry() -> CaptureGeometry {
        CaptureGeometry {
            window_origin: (0.0, 0.0),
            window_size: (100.0, 100.0),
            image_size: (100, 100),
        }
    }
    #[test]
    fn omitted_branches_do_not_invent_query_ancestors() {
        let tree = Node::group(vec![
            Node::group(vec![Node::control("Unrelated button")]),
            Node::group(vec![Node::group(vec![Node::control("Wanted field")])]),
        ]);
        let (elements, _) = poll(
            Instant::now() + Duration::from_secs(3),
            |deadline, elements, entries| {
                walk(
                    tree.clone(),
                    Depth::default(),
                    geometry(),
                    deadline,
                    elements,
                    entries,
                )
            },
        )
        .unwrap();
        let filtered = filter_elements(&elements, "Wanted");
        assert_eq!(
            filtered
                .iter()
                .map(|e| e.label.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["Wanted field"]
        );
        assert_eq!(
            elements.iter().map(|e| e.depth).collect::<Vec<_>>(),
            vec![0, 0]
        );
        // Positive control: a retained parent really is kept for its descendant.
        let parent = Node {
            label: Some("Retained parent"),
            delay: Duration::ZERO,
            children: vec![Node::group(vec![Node::control("Wanted child")])],
        };
        let (elements, _) = poll(
            Instant::now() + Duration::from_secs(3),
            |deadline, elements, entries| {
                walk(
                    parent.clone(),
                    Depth::default(),
                    geometry(),
                    deadline,
                    elements,
                    entries,
                )
            },
        )
        .unwrap();
        assert_eq!(filter_elements(&elements, "Wanted").len(), 2);
        assert_eq!(
            elements.iter().map(|e| e.depth).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }
    #[test]
    fn slow_empty_traversal_stops_within_one_read_of_the_wait_budget() {
        let read_budget = Duration::from_secs(2);
        let tree = Node {
            label: None,
            delay: read_budget,
            children: vec![Node {
                label: None,
                delay: read_budget,
                children: vec![Node {
                    label: None,
                    delay: read_budget,
                    children: vec![],
                }],
            }],
        };
        let start = Instant::now();
        let budget = Duration::from_secs(3);
        let (elements, _) = poll(start + budget, |deadline, elements, entries| {
            walk(
                tree.clone(),
                Depth::default(),
                geometry(),
                deadline,
                elements,
                entries,
            )
        })
        .unwrap();
        assert!(
            elements.is_empty(),
            "the empty-tree observation must stay empty"
        );
        assert!(
            start.elapsed() < budget + read_budget + Duration::from_millis(200),
            "elapsed {:?} exceeded wait plus one read",
            start.elapsed()
        );
    }
    #[test]
    fn no_retry_starts_after_the_final_sleep() {
        let deadline = Instant::now() + Duration::from_millis(50);
        let mut calls = 0;
        poll::<()>(deadline, |deadline, _, _| {
            assert!(Instant::now() < deadline, "retry began after the deadline");
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 1);
    }
}
