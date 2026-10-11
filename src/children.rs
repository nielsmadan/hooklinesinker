use crate::protocol::Phase;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;

pub(crate) type Children = BTreeMap<String, Phase>;
pub(crate) const MAX_CHILDREN: usize = 64;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChildEvent<'a> {
    pub event: &'a str,
    pub mapped: Phase,
    pub running: bool,
    pub awaiting_user: bool,
}

pub(crate) fn child_key(native_id: &str) -> String {
    let digest = Sha256::digest(native_id.as_bytes());
    digest[..8]
        .iter()
        .fold(String::with_capacity(16), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to String cannot fail");
            hex
        })
}

const fn holds_working(phase: Phase) -> bool {
    matches!(phase, Phase::Working | Phase::Compacting)
}

pub(crate) fn aggregate_phase(own: Phase, children: &Children, held: bool) -> Phase {
    if own == Phase::Permission || children.values().any(|phase| *phase == Phase::Permission) {
        Phase::Permission
    } else if matches!(own, Phase::Idle | Phase::Unknown)
        && (held || children.values().copied().any(holds_working))
    {
        Phase::Working
    } else {
        own
    }
}

pub(crate) fn completion_holds_root(
    own: Phase,
    before: Option<Phase>,
    after: Phase,
    children: &Children,
) -> bool {
    before.is_some_and(|phase| phase != Phase::Idle)
        && after == Phase::Idle
        && matches!(
            aggregate_phase(own, children, false),
            Phase::Idle | Phase::Unknown
        )
}

pub(crate) fn child_phase(child_stop: &str, child: ChildEvent<'_>) -> Phase {
    if child.event == child_stop || !child.running {
        Phase::Idle
    } else if child.awaiting_user || child.mapped == Phase::Permission {
        Phase::Permission
    } else if holds_working(child.mapped) {
        Phase::Working
    } else {
        Phase::Idle
    }
}

// A child seen only via a Permission event; never holds the root working.
const PROVISIONAL_PREFIX: &str = "prov:";

fn provisional_key(key: &str) -> String {
    format!("{PROVISIONAL_PREFIX}{key}")
}

fn insert_with_eviction(mut children: Children, key: &str, phase: Phase) -> Children {
    if children.len() >= MAX_CHILDREN {
        let Some(finished) = children.iter().find_map(|(candidate, existing)| {
            (*existing == Phase::Idle).then(|| candidate.clone())
        }) else {
            return children;
        };
        children.remove(&finished);
    }
    children.insert(key.to_string(), phase);
    children
}

pub(crate) fn with_child(children: &Children, key: &str, phase: Phase, create: bool) -> Children {
    let mut updated = children.clone();
    if updated.contains_key(key) {
        updated.insert(key.to_string(), phase);
        return updated;
    }
    let provisional = provisional_key(key);
    if create {
        updated.remove(&provisional);
        return insert_with_eviction(updated, key, phase);
    }
    if let std::collections::btree_map::Entry::Occupied(mut entry) =
        updated.entry(provisional.clone())
    {
        if phase == Phase::Permission {
            entry.insert(phase);
        } else {
            entry.remove();
        }
        return updated;
    }
    if phase == Phase::Permission {
        return insert_with_eviction(updated, &provisional, phase);
    }
    updated
}

pub(crate) fn all_idle(children: &Children) -> Children {
    children
        .keys()
        .map(|key| (key.clone(), Phase::Idle))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn children(entries: &[(&str, Phase)]) -> Children {
        entries
            .iter()
            .map(|(key, phase)| ((*key).to_string(), *phase))
            .collect()
    }

    fn event(event: &str, mapped: Phase) -> ChildEvent<'_> {
        ChildEvent {
            event,
            mapped,
            running: true,
            awaiting_user: false,
        }
    }

    #[test]
    fn aggregate_phase_follows_the_root_unless_a_child_needs_it() {
        let cases = [
            (Phase::Idle, children(&[]), Phase::Idle),
            (
                Phase::Idle,
                children(&[("a", Phase::Working)]),
                Phase::Working,
            ),
            (
                Phase::Unknown,
                children(&[("a", Phase::Compacting)]),
                Phase::Working,
            ),
            (
                Phase::Compacting,
                children(&[("a", Phase::Working)]),
                Phase::Compacting,
            ),
            (
                Phase::Working,
                children(&[("a", Phase::Permission)]),
                Phase::Permission,
            ),
            (
                Phase::Permission,
                children(&[("a", Phase::Idle)]),
                Phase::Permission,
            ),
            (Phase::Idle, children(&[("a", Phase::Idle)]), Phase::Idle),
        ];
        for (own, children, expected) in cases {
            assert_eq!(
                aggregate_phase(own, &children, false),
                expected,
                "{own:?} {children:?}"
            );
        }
    }

    #[test]
    fn a_held_root_aggregates_working_unless_something_waits_on_the_user() {
        let finished = children(&[("a", Phase::Idle)]);
        assert_eq!(
            aggregate_phase(Phase::Idle, &finished, true),
            Phase::Working
        );
        assert_eq!(
            aggregate_phase(Phase::Unknown, &finished, true),
            Phase::Working
        );
        assert_eq!(
            aggregate_phase(Phase::Idle, &children(&[("a", Phase::Permission)]), true),
            Phase::Permission
        );
        assert_eq!(
            aggregate_phase(Phase::Compacting, &finished, true),
            Phase::Compacting
        );
    }

    #[test]
    fn only_the_completion_that_idles_the_root_holds_it() {
        let finished = children(&[("a", Phase::Idle), ("b", Phase::Idle)]);
        let one_running = children(&[("a", Phase::Idle), ("b", Phase::Working)]);
        let cases = [
            (
                Phase::Idle,
                Some(Phase::Working),
                Phase::Idle,
                &finished,
                true,
            ),
            (
                Phase::Unknown,
                Some(Phase::Working),
                Phase::Idle,
                &finished,
                true,
            ),
            (
                Phase::Idle,
                Some(Phase::Permission),
                Phase::Idle,
                &finished,
                true,
            ),
            (
                Phase::Idle,
                Some(Phase::Working),
                Phase::Idle,
                &one_running,
                false,
            ),
            (
                Phase::Working,
                Some(Phase::Working),
                Phase::Idle,
                &finished,
                false,
            ),
            (
                Phase::Idle,
                Some(Phase::Idle),
                Phase::Idle,
                &finished,
                false,
            ),
            (Phase::Idle, None, Phase::Idle, &finished, false),
            (
                Phase::Idle,
                Some(Phase::Working),
                Phase::Working,
                &finished,
                false,
            ),
        ];
        for (own, before, after, children, expected) in cases {
            assert_eq!(
                completion_holds_root(own, before, after, children),
                expected,
                "{own:?} {before:?} -> {after:?} {children:?}"
            );
        }
    }

    #[test]
    fn child_phase_follows_the_child_event() {
        let stop = "SubagentStop";
        assert_eq!(
            child_phase(stop, event("SubagentStop", Phase::Working)),
            Phase::Idle
        );
        assert_eq!(
            child_phase(stop, event("PreToolUse", Phase::Working)),
            Phase::Working
        );
        assert_eq!(
            child_phase(stop, event("PreCompact", Phase::Compacting)),
            Phase::Working
        );
        assert_eq!(
            child_phase(stop, event("PermissionRequest", Phase::Permission)),
            Phase::Permission
        );
        assert_eq!(
            child_phase(
                stop,
                ChildEvent {
                    awaiting_user: true,
                    ..event("PreToolUse", Phase::Idle)
                }
            ),
            Phase::Permission
        );
        assert_eq!(child_phase(stop, event("Stop", Phase::Idle)), Phase::Idle);
        assert_eq!(
            child_phase(
                stop,
                ChildEvent {
                    running: false,
                    ..event("SessionEnd", Phase::Working)
                }
            ),
            Phase::Idle
        );
    }

    #[test]
    fn only_start_events_create_children() {
        let existing = children(&[("a", Phase::Idle)]);
        assert_eq!(with_child(&existing, "b", Phase::Working, false), existing);
        assert_eq!(
            with_child(&existing, "a", Phase::Working, false),
            children(&[("a", Phase::Working)])
        );
        assert_eq!(
            with_child(&existing, "b", Phase::Working, true),
            children(&[("a", Phase::Idle), ("b", Phase::Working)])
        );
    }

    #[test]
    fn with_child_evicts_one_finished_child_at_capacity() {
        let mut full: Children = (0..MAX_CHILDREN)
            .map(|index| (format!("{index:02}"), Phase::Working))
            .collect();
        assert_eq!(with_child(&full, "new", Phase::Working, true), full);

        full.insert("10".into(), Phase::Idle);
        let updated = with_child(&full, "new", Phase::Working, true);
        assert_eq!(updated.len(), MAX_CHILDREN);
        assert_eq!(updated.get("new"), Some(&Phase::Working));
        assert_eq!(updated.get("10"), None);
        assert_eq!(updated.get("11"), Some(&Phase::Working));
    }

    #[test]
    fn an_untracked_childs_permission_creates_a_provisional_entry() {
        let empty = children(&[]);
        let updated = with_child(&empty, "w", Phase::Permission, false);
        assert_eq!(updated.len(), 1);
        assert_eq!(
            aggregate_phase(Phase::Working, &updated, false),
            Phase::Permission
        );
    }

    #[test]
    fn an_untracked_childs_other_events_create_nothing() {
        let empty = children(&[]);
        assert_eq!(with_child(&empty, "w", Phase::Working, false), empty);
        assert_eq!(with_child(&empty, "w", Phase::Idle, false), empty);
    }

    #[test]
    fn a_provisional_entrys_next_non_permission_event_clears_it() {
        let provisional = with_child(&children(&[]), "w", Phase::Permission, false);
        assert_eq!(
            with_child(&provisional, "w", Phase::Working, false),
            children(&[])
        );
    }

    #[test]
    fn a_provisional_entry_stays_through_repeated_permission_events() {
        let provisional = with_child(&children(&[]), "w", Phase::Permission, false);
        assert_eq!(
            with_child(&provisional, "w", Phase::Permission, false),
            provisional
        );
    }

    #[test]
    fn a_provisional_entry_becomes_tracked_on_the_childs_start_event() {
        let provisional = with_child(&children(&[]), "w", Phase::Permission, false);
        assert_eq!(
            with_child(&provisional, "w", Phase::Working, true),
            children(&[("w", Phase::Working)])
        );
    }

    #[test]
    fn provisional_entries_never_hold_the_root_working() {
        let provisional = with_child(&children(&[]), "w", Phase::Permission, false);
        assert_eq!(
            aggregate_phase(Phase::Idle, &provisional, false),
            Phase::Permission
        );
        assert!(
            provisional
                .values()
                .all(|phase| !matches!(phase, Phase::Working | Phase::Compacting))
        );
    }

    #[test]
    fn child_keys_are_stable_short_digests() {
        let key = child_key("worker-1");
        assert_eq!(key, child_key("worker-1"));
        assert_ne!(key, child_key("worker-2"));
        assert_ne!(key, "worker-1");
        assert_eq!(key.len(), 16);
        assert!(
            key.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }
}
