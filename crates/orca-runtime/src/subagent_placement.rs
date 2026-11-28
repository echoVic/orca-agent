//! Where a subagent runs: detached in a worker process, or in this process.

/// What the decision to detach a child depends on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChildPlacement {
    /// The child was submitted in `Async` mode.
    pub(crate) mode_async: bool,
    /// The task registry is shared between processes.
    pub(crate) registry_shared: bool,
    /// The parent has a fence a worker can be bound to.
    pub(crate) parent_fence: bool,
    /// The parent has an activity ingress a worker can report to.
    pub(crate) activity_ingress: bool,
    /// The host asked for its subagents to stay in the process.
    pub(crate) host_keeps_children: bool,
    /// The child can run on a hosted thread of its own.
    pub(crate) qualifies_for_thread: bool,
}

/// `true` when the child is launched as a detached worker process.
pub(crate) fn detaches(placement: ChildPlacement) -> bool {
    let detached_today = placement.mode_async
        && placement.registry_shared
        && placement.parent_fence
        && placement.activity_ingress;
    detached_today && !(placement.host_keeps_children && placement.qualifies_for_thread)
}

#[cfg(test)]
mod tests {
    use super::{ChildPlacement, detaches};

    const DETACHED_TODAY: ChildPlacement = ChildPlacement {
        mode_async: true,
        registry_shared: true,
        parent_fence: true,
        activity_ingress: true,
        host_keeps_children: false,
        qualifies_for_thread: true,
    };

    #[test]
    fn a_host_that_does_not_ask_detaches_as_before() {
        assert!(detaches(DETACHED_TODAY));
        for placement in [
            ChildPlacement {
                mode_async: false,
                ..DETACHED_TODAY
            },
            ChildPlacement {
                registry_shared: false,
                ..DETACHED_TODAY
            },
            ChildPlacement {
                parent_fence: false,
                ..DETACHED_TODAY
            },
            ChildPlacement {
                activity_ingress: false,
                ..DETACHED_TODAY
            },
        ] {
            assert!(!detaches(placement), "{placement:?}");
        }
    }

    #[test]
    fn a_host_that_keeps_its_children_keeps_those_that_can_have_a_thread() {
        let kept = ChildPlacement {
            host_keeps_children: true,
            ..DETACHED_TODAY
        };
        assert!(!detaches(kept));
    }

    #[test]
    fn a_child_that_cannot_have_a_thread_is_detached_even_then() {
        let no_thread = ChildPlacement {
            host_keeps_children: true,
            qualifies_for_thread: false,
            ..DETACHED_TODAY
        };
        assert!(detaches(no_thread));
    }
}
