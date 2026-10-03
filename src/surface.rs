/// Which surfaces a record has. The home surface always exists on the record's monitor; the
/// travellers are on other monitors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SurfaceState {
    Home,
    /// Travellers on these monitors, ascending.
    Straddling(Vec<usize>),
    /// The surface on `monitor` becomes the record's overlay at its first configure. The
    /// travellers on `others` go with the home surface.
    Landing {
        monitor: usize,
        others: Vec<usize>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SurfaceEvent {
    /// The monitors other than home that the dragged rectangle touches, ascending.
    Touch(Vec<usize>),
    Configured(usize),
    ReleaseHome,
    /// The record lands on `monitor`: a drag ended there, or a key change moves a record that
    /// has only its home surface.
    ReleaseAt {
        monitor: usize,
        configured: bool,
    },
    TearDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceAction {
    Create(usize),
    Destroy(usize),
    Present(usize),
    Commit(usize),
}

/// The surfaces of one record after `event`, and the actions that take them there. Drag
/// events during a pending landing change nothing.
pub(crate) fn surface_step(
    state: &SurfaceState,
    event: SurfaceEvent,
) -> (SurfaceState, Vec<SurfaceAction>) {
    use SurfaceAction::{Commit, Create, Destroy, Present};
    let travellers: Vec<usize> = match state {
        SurfaceState::Home => Vec::new(),
        SurfaceState::Straddling(set) => set.clone(),
        SurfaceState::Landing { monitor, others } => std::iter::once(*monitor)
            .chain(others.iter().copied())
            .collect(),
    };
    let destroy_all = || travellers.iter().map(|m| Destroy(*m)).collect();
    let straddle = |set: Vec<usize>| {
        if set.is_empty() {
            SurfaceState::Home
        } else {
            SurfaceState::Straddling(set)
        }
    };
    match (state, event) {
        (SurfaceState::Landing { .. }, SurfaceEvent::Touch(_) | SurfaceEvent::ReleaseHome) => {
            (state.clone(), Vec::new())
        }
        (SurfaceState::Landing { .. }, SurfaceEvent::ReleaseAt { .. }) => {
            (state.clone(), Vec::new())
        }
        (_, SurfaceEvent::Touch(next)) => {
            let mut actions: Vec<SurfaceAction> = travellers
                .iter()
                .filter(|m| !next.contains(m))
                .map(|m| Destroy(*m))
                .collect();
            actions.extend(
                next.iter()
                    .filter(|m| !travellers.contains(m))
                    .map(|m| Create(*m)),
            );
            (straddle(next), actions)
        }
        (_, SurfaceEvent::ReleaseHome) => (SurfaceState::Home, destroy_all()),
        (
            _,
            SurfaceEvent::ReleaseAt {
                monitor,
                configured,
            },
        ) => {
            let exists = travellers.contains(&monitor);
            let others: Vec<usize> = travellers
                .iter()
                .copied()
                .filter(|m| *m != monitor)
                .collect();
            if exists && configured {
                let mut actions: Vec<SurfaceAction> = others.iter().map(|m| Destroy(*m)).collect();
                actions.push(Commit(monitor));
                (SurfaceState::Home, actions)
            } else {
                let actions = if exists {
                    Vec::new()
                } else {
                    vec![Create(monitor)]
                };
                (SurfaceState::Landing { monitor, others }, actions)
            }
        }
        (SurfaceState::Landing { monitor, others }, SurfaceEvent::Configured(at)) => {
            if at == *monitor {
                let mut actions = vec![Present(at)];
                actions.extend(others.iter().map(|m| Destroy(*m)));
                actions.push(Commit(at));
                (SurfaceState::Home, actions)
            } else if others.contains(&at) {
                (state.clone(), vec![Present(at)])
            } else {
                (state.clone(), Vec::new())
            }
        }
        (_, SurfaceEvent::Configured(at)) => {
            let actions = if travellers.contains(&at) {
                vec![Present(at)]
            } else {
                Vec::new()
            };
            (state.clone(), actions)
        }
        (_, SurfaceEvent::TearDown) => (SurfaceState::Home, destroy_all()),
    }
}

/// The monitor whose usable area bounds a record's geometry: the landing monitor while a
/// landing is pending, else the record's own.
pub(crate) fn bound_monitor(state: &SurfaceState, monitor: usize) -> usize {
    match state {
        SurfaceState::Landing { monitor, .. } => *monitor,
        SurfaceState::Home | SurfaceState::Straddling(_) => monitor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_monitor_cases() {
        let cases = [
            ("home", SurfaceState::Home, 0, 0),
            ("straddling", SurfaceState::Straddling(vec![1, 2]), 0, 0),
            (
                "landing",
                SurfaceState::Landing {
                    monitor: 2,
                    others: vec![1],
                },
                0,
                2,
            ),
        ];
        for (name, state, monitor, want) in cases {
            assert_eq!(bound_monitor(&state, monitor), want, "{name}");
        }
    }

    #[test]
    fn surface_step_cases() {
        use SurfaceAction::{Commit, Create, Destroy, Present};
        use SurfaceEvent::{Configured, ReleaseAt, ReleaseHome, TearDown, Touch};
        use SurfaceState::{Home, Landing, Straddling};
        let landing = |monitor, others: &[usize]| Landing {
            monitor,
            others: others.to_vec(),
        };
        type Case<'a> = (
            &'a str,
            SurfaceState,
            SurfaceEvent,
            SurfaceState,
            Vec<SurfaceAction>,
        );
        let cases: Vec<Case> = vec![
            ("home, still home", Home, Touch(vec![]), Home, vec![]),
            (
                "home to straddling",
                Home,
                Touch(vec![1]),
                Straddling(vec![1]),
                vec![Create(1)],
            ),
            (
                "corner of three monitors",
                Home,
                Touch(vec![1, 2]),
                Straddling(vec![1, 2]),
                vec![Create(1), Create(2)],
            ),
            (
                "corner left for one monitor",
                Straddling(vec![1, 2]),
                Touch(vec![2]),
                Straddling(vec![2]),
                vec![Destroy(1)],
            ),
            (
                "third monitor entered",
                Straddling(vec![1]),
                Touch(vec![2]),
                Straddling(vec![2]),
                vec![Destroy(1), Create(2)],
            ),
            (
                "third monitor left",
                Straddling(vec![2]),
                Touch(vec![]),
                Home,
                vec![Destroy(2)],
            ),
            (
                "same monitors",
                Straddling(vec![1]),
                Touch(vec![1]),
                Straddling(vec![1]),
                vec![],
            ),
            (
                "drag events while landing change nothing",
                landing(1, &[]),
                Touch(vec![]),
                landing(1, &[]),
                vec![],
            ),
            ("release on home from home", Home, ReleaseHome, Home, vec![]),
            (
                "release on home destroys every traveller",
                Straddling(vec![1, 2]),
                ReleaseHome,
                Home,
                vec![Destroy(1), Destroy(2)],
            ),
            (
                "release on a configured traveller commits",
                Straddling(vec![1, 2]),
                ReleaseAt {
                    monitor: 1,
                    configured: true,
                },
                Home,
                vec![Destroy(2), Commit(1)],
            ),
            (
                "release before the traveller's configure",
                Straddling(vec![1, 2]),
                ReleaseAt {
                    monitor: 1,
                    configured: false,
                },
                landing(1, &[2]),
                vec![],
            ),
            (
                "release where no traveller exists yet",
                Straddling(vec![2]),
                ReleaseAt {
                    monitor: 1,
                    configured: false,
                },
                landing(1, &[2]),
                vec![Create(1)],
            ),
            (
                "release or key change onto another monitor from home",
                Home,
                ReleaseAt {
                    monitor: 1,
                    configured: false,
                },
                landing(1, &[]),
                vec![Create(1)],
            ),
            (
                "configure after the release commits",
                landing(1, &[2]),
                Configured(1),
                Home,
                vec![Present(1), Destroy(2), Commit(1)],
            ),
            (
                "another traveller's configure while landing",
                landing(1, &[2]),
                Configured(2),
                landing(1, &[2]),
                vec![Present(2)],
            ),
            (
                "configure of a traveller during the drag",
                Straddling(vec![1]),
                Configured(1),
                Straddling(vec![1]),
                vec![Present(1)],
            ),
            (
                "configure of an unknown monitor during the drag",
                Straddling(vec![1]),
                Configured(2),
                Straddling(vec![1]),
                vec![],
            ),
            (
                "configure of an unknown monitor while landing",
                landing(1, &[2]),
                Configured(3),
                landing(1, &[2]),
                vec![],
            ),
            (
                "release on home while landing",
                landing(1, &[2]),
                ReleaseHome,
                landing(1, &[2]),
                vec![],
            ),
            (
                "release elsewhere while landing",
                landing(1, &[]),
                ReleaseAt {
                    monitor: 2,
                    configured: false,
                },
                landing(1, &[]),
                vec![],
            ),
            ("hide from home", Home, TearDown, Home, vec![]),
            (
                "hide during a drag",
                Straddling(vec![1, 2]),
                TearDown,
                Home,
                vec![Destroy(1), Destroy(2)],
            ),
            (
                "hide while landing",
                landing(1, &[2]),
                TearDown,
                Home,
                vec![Destroy(1), Destroy(2)],
            ),
        ];
        for (name, state, event, next, actions) in cases {
            assert_eq!(surface_step(&state, event), (next, actions), "{name}");
        }
    }
}
