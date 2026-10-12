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
    window_frame_control: bool,
}

const EMPTY_TREE_RETRY_BUDGET: Duration = Duration::from_secs(3);
const FULL_TREE_WALK_BUDGET: Duration = Duration::from_secs(10);

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
    fn is_content(entry: &Self::Entry) -> bool;
    fn retained(
        &self,
        geometry: CaptureGeometry,
        deadline: Instant,
    ) -> Result<Option<(Element, Self::Entry)>, DriverError>;
    fn children_until(&self, deadline: Instant) -> Result<Vec<Self>, DriverError>;
}

impl TreeNode for AxElement {
    type Entry = TableEntry;
    fn is_content(entry: &TableEntry) -> bool {
        !entry.window_frame_control
    }
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
        let role = read!(self.optional_attribute_string("AXRole")).unwrap_or_default();
        let retained_role = ROLES.contains(&role.as_str());
        let subrole = read!(self.optional_attribute_string("AXSubrole"));
        let secure = super::ax::secure_text_role(&role, subrole.as_deref());
        let window_frame_control = role == "AXButton"
            && matches!(
                subrole.as_deref(),
                Some("AXCloseButton" | "AXMinimizeButton" | "AXZoomButton" | "AXFullScreenButton")
            );
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
                window_frame_control,
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

struct WalkBudget {
    deadline: Instant,
    full_deadline: Instant,
    truncated: bool,
}
impl WalkBudget {
    fn expired(&mut self) -> bool {
        if Instant::now() >= self.deadline {
            self.truncated = true;
            true
        } else {
            false
        }
    }
}
fn walk<N: TreeNode>(
    element: N,
    depth: Depth,
    geometry: CaptureGeometry,
    deadline: Instant,
    elements: &mut Vec<Element>,
    entries: &mut Vec<N::Entry>,
) -> Result<bool, DriverError> {
    let mut budget = WalkBudget {
        deadline,
        full_deadline: Instant::now() + FULL_TREE_WALK_BUDGET,
        truncated: false,
    };
    walk_until(element, depth, geometry, &mut budget, elements, entries)?;
    Ok(budget.truncated)
}
fn walk_until<N: TreeNode>(
    element: N,
    depth: Depth,
    geometry: CaptureGeometry,
    budget: &mut WalkBudget,
    elements: &mut Vec<Element>,
    entries: &mut Vec<N::Entry>,
) -> Result<(), DriverError> {
    if depth.ax > 30 || budget.expired() {
        return Ok(());
    }
    let mut child_depth = Depth {
        ax: depth.ax + 1,
        retained: depth.retained,
    };
    let retained = match element.retained(geometry, budget.deadline) {
        Err(failure) if depth.ax > 0 && failure.kind == crate::protocol::ErrorKind::Stale => {
            return Ok(());
        }
        result => result?,
    };
    if let Some((mut retained, entry)) = retained {
        // Once real content appears, complete this walk with its independent budget.
        if N::is_content(&entry) {
            budget.deadline = budget.full_deadline;
        }
        retained.id = elements.len() as u32 + 1;
        retained.depth = depth.retained;
        child_depth.retained += 1;
        elements.push(retained);
        entries.push(entry);
    }
    if depth.ax < 30 && !budget.expired() {
        let children = match element.children_until(budget.deadline) {
            Err(failure) if depth.ax > 0 && failure.kind == crate::protocol::ErrorKind::Stale => {
                return Ok(());
            }
            result => result?,
        };
        for child in children {
            if budget.expired() {
                break;
            }
            walk_until(child, child_depth, geometry, budget, elements, entries)?;
        }
        budget.expired();
    }
    Ok(())
}

fn poll<E>(
    deadline: Instant,
    mut traversal: impl FnMut(Instant, &mut Vec<Element>, &mut Vec<E>) -> Result<bool, DriverError>,
    ready: impl Fn(&[E]) -> bool,
) -> Result<(Vec<Element>, Vec<E>, bool), DriverError> {
    let mut elements = vec![];
    let mut entries = vec![];
    let mut truncated = false;
    loop {
        // In particular, don't start a retry after the final capped sleep.
        if Instant::now() >= deadline {
            return Ok((elements, entries, truncated));
        }
        // Each retry is a fresh snapshot, not an append to the frame controls
        // from the previous traversal. Keep the last nonempty snapshot if the
        // budget expires before a retry can retain any elements.
        let mut next_elements = vec![];
        let mut next_entries = vec![];
        let next_truncated = traversal(deadline, &mut next_elements, &mut next_entries)?;
        let is_ready = ready(&next_entries);
        if !next_elements.is_empty() {
            elements = next_elements;
            entries = next_entries;
            truncated = next_truncated;
        } else {
            truncated |= next_truncated;
        }
        if is_ready || Instant::now() >= deadline {
            return Ok((elements, entries, truncated));
        }
        thread::sleep(
            Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn has_element_tree(entries: &[TableEntry]) -> bool {
    entries.iter().any(AxElement::is_content)
}

pub(super) struct CollectedTree {
    pub elements: Vec<Element>,
    pub entries: Vec<TableEntry>,
    pub omitted: u32,
    pub no_element_tree: bool,
    pub truncated: bool,
}

pub(super) fn collect(
    window: &AxElement,
    geometry: CaptureGeometry,
    query: Option<&str>,
    max_elements: u32,
) -> Result<CollectedTree, DriverError> {
    let deadline = Instant::now() + EMPTY_TREE_RETRY_BUDGET;
    let (mut elements, entries, truncated) = poll(
        deadline,
        |deadline, elements, entries| {
            walk(
                window.clone(),
                Depth::default(),
                geometry,
                deadline,
                elements,
                entries,
            )
        },
        has_element_tree,
    )?;
    let no_element_tree = !has_element_tree(&entries);
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
    Ok(CollectedTree {
        elements,
        entries: selected,
        omitted,
        no_element_tree,
        truncated,
    })
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

    fn button(id: u32, label: &str) -> Element {
        Element {
            id,
            depth: 0,
            role: "button".into(),
            label: Some(label.into()),
            value: None,
            secure: false,
            frame: [0; 4],
        }
    }

    #[test]
    fn frame_buttons_are_retried_until_content_arrives_without_duplicates() {
        let mut calls = 0;
        let (elements, entries, _) = poll(
            Instant::now() + Duration::from_secs(1),
            |_, elements, entries| {
                calls += 1;
                elements.push(button(1, "Close"));
                entries.push(true);
                if calls == 2 {
                    elements.push(button(2, "Content"));
                    entries.push(false);
                }
                Ok(false)
            },
            |entries| entries.iter().any(|frame: &bool| !frame),
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(
            elements
                .iter()
                .map(|element| element.id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(entries, [true, false]);
    }

    #[test]
    fn a_title_bar_only_window_keeps_its_buttons_after_the_budget() {
        let started = Instant::now();
        let budget = Duration::from_millis(50);
        let mut calls = 0;
        let (elements, entries, _) = poll(
            started + budget,
            |_, elements, entries| {
                calls += 1;
                elements.push(button(1, "Close"));
                entries.push(true);
                Ok(false)
            },
            |entries| entries.iter().any(|frame: &bool| !frame),
        )
        .unwrap();
        assert!(started.elapsed() >= budget);
        assert_eq!(calls, 1, "no retry after the final capped sleep");
        assert_eq!(elements[0].label.as_deref(), Some("Close"));
        assert_eq!(entries, [true]);
    }

    #[test]
    fn a_content_button_returns_in_the_first_traversal() {
        let mut calls = 0;
        let (_, entries, _) = poll(
            Instant::now() + Duration::from_secs(3),
            |_, elements, entries| {
                calls += 1;
                elements.push(button(1, "Content"));
                entries.push(false);
                Ok(false)
            },
            |entries| entries.iter().any(|frame: &bool| !frame),
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(entries, [false]);
    }

    #[test]
    fn an_empty_final_traversal_keeps_the_last_frame_snapshot() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut calls = 0;
        let (elements, entries, _) = poll(
            deadline,
            |_, elements, entries| {
                calls += 1;
                if calls == 1 {
                    elements.push(button(1, "Close"));
                    entries.push(true);
                } else {
                    thread::sleep(deadline.saturating_duration_since(Instant::now()));
                }
                Ok(false)
            },
            |entries| entries.iter().any(|frame: &bool| !frame),
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(elements[0].label.as_deref(), Some("Close"));
        assert_eq!(entries, [true]);
    }

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
        fn is_content(_: &()) -> bool {
            true
        }
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
    #[derive(Clone)]
    struct FaultNode {
        node: Node,
        kind: crate::protocol::ErrorKind,
        on_children: bool,
    }
    impl TreeNode for FaultNode {
        type Entry = ();
        fn is_content(_: &()) -> bool {
            true
        }
        fn retained(
            &self,
            geometry: CaptureGeometry,
            deadline: Instant,
        ) -> Result<Option<(Element, ())>, DriverError> {
            if self.node.label == Some("Bad") && !self.on_children {
                return Err(super::super::error(self.kind, "unavailable node"));
            }
            self.node.retained(geometry, deadline)
        }
        fn children_until(&self, deadline: Instant) -> Result<Vec<Self>, DriverError> {
            if self.node.label == Some("Bad") && self.on_children {
                return Err(super::super::error(self.kind, "unavailable children"));
            }
            Ok(self
                .node
                .children_until(deadline)?
                .into_iter()
                .map(|node| Self {
                    node,
                    kind: self.kind,
                    on_children: self.on_children,
                })
                .collect())
        }
    }

    #[test]
    fn a_stale_child_does_not_hide_good_siblings_but_root_and_timeout_errors_fail() {
        use crate::protocol::ErrorKind;
        for on_children in [false, true] {
            let tree = FaultNode {
                node: Node::group(vec![
                    Node::control("First"),
                    Node::control("Bad"),
                    Node::control("Last"),
                ]),
                kind: ErrorKind::Stale,
                on_children,
            };
            let mut elements = vec![];
            walk(
                tree.clone(),
                Depth::default(),
                geometry(),
                Instant::now() + Duration::from_secs(1),
                &mut elements,
                &mut vec![],
            )
            .unwrap();
            assert!(elements.iter().any(|e| e.label.as_deref() == Some("First")));
            assert!(elements.iter().any(|e| e.label.as_deref() == Some("Last")));
            assert_eq!(
                walk(
                    FaultNode {
                        node: Node::control("Bad"),
                        ..tree.clone()
                    },
                    Depth::default(),
                    geometry(),
                    Instant::now() + Duration::from_secs(1),
                    &mut vec![],
                    &mut vec![]
                )
                .unwrap_err()
                .kind,
                ErrorKind::Stale
            );
            assert_eq!(
                walk(
                    FaultNode {
                        kind: ErrorKind::Failed,
                        ..tree
                    },
                    Depth::default(),
                    geometry(),
                    Instant::now() + Duration::from_secs(1),
                    &mut vec![],
                    &mut vec![]
                )
                .unwrap_err()
                .kind,
                ErrorKind::Failed,
                "timeouts must remain fatal"
            );
        }
    }

    #[test]
    fn omitted_branches_do_not_invent_query_ancestors() {
        let tree = Node::group(vec![
            Node::group(vec![Node::control("Unrelated button")]),
            Node::group(vec![Node::group(vec![Node::control("Wanted field")])]),
        ]);
        let (elements, _, _) = poll(
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
            |entries| !entries.is_empty(),
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
        let (elements, _, _) = poll(
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
            |entries| !entries.is_empty(),
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
        let (elements, _, _) = poll(
            start + budget,
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
            |entries| !entries.is_empty(),
        )
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
        poll::<()>(
            deadline,
            |deadline, _, _| {
                assert!(Instant::now() < deadline, "retry began after the deadline");
                calls += 1;
                Ok(false)
            },
            |entries| !entries.is_empty(),
        )
        .unwrap();
        assert_eq!(calls, 1);
    }

    #[test]
    fn full_walk_cutoff_is_reported_without_extending_the_deadline() {
        let tree = Node {
            label: Some("Content"),
            delay: Duration::ZERO,
            children: vec![
                Node {
                    delay: Duration::from_millis(30),
                    ..Node::control("Slow")
                },
                Node::control("Last"),
            ],
        };
        for (budget_ms, expected_cutoff, expected_count) in [(20, true, 2), (100, false, 3)] {
            let start = Instant::now();
            let mut budget = WalkBudget {
                deadline: start + Duration::from_millis(5),
                full_deadline: start + Duration::from_millis(budget_ms),
                truncated: false,
            };
            let mut elements = vec![];
            walk_until(
                tree.clone(),
                Depth::default(),
                geometry(),
                &mut budget,
                &mut elements,
                &mut vec![],
            )
            .unwrap();
            assert_eq!(budget.truncated, expected_cutoff);
            assert_eq!(elements.len(), expected_count);
        }
    }
    #[test]
    fn a_full_tree_walk_outlives_the_empty_tree_retry_budget() {
        let tree = Node {
            label: Some("Content parent"),
            delay: Duration::ZERO,
            children: vec![
                Node {
                    delay: Duration::from_secs(2),
                    ..Node::control("First")
                },
                Node {
                    delay: Duration::from_secs(2),
                    ..Node::control("Second")
                },
                Node::control("Last"),
            ],
        };
        let (elements, _, _) = poll(
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
            |entries| !entries.is_empty(),
        )
        .unwrap();
        assert_eq!(
            elements
                .iter()
                .map(|e| e.label.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["Content parent", "First", "Second", "Last"]
        );
    }
}

#[cfg(test)]
#[test]
fn title_bar_buttons_do_not_count_as_an_element_tree() {
    let application = AxElement::application(i32::MAX).unwrap();
    let mut entries = vec![TableEntry {
        element: application,
        role: "AXButton".into(),
        label: None,
        secure: false,
        window_frame_control: true,
    }];
    assert!(!has_element_tree(&entries));
    entries[0].window_frame_control = false;
    assert!(has_element_tree(&entries));
}
