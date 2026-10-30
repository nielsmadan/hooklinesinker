use crate::agents::SubagentSource;
use crate::children::Children;
use crate::protocol::{Activity, Phase, SessionIdentity};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Field<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

impl<T: Clone> Field<T> {
    pub(crate) fn apply(&self, previous: Option<T>) -> Option<T> {
        match self {
            Self::Keep => previous,
            Self::Clear => None,
            Self::Set(value) => Some(value.clone()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Details {
    pub title: Field<String>,
    pub model: Field<String>,
    pub effort: Field<String>,
    pub permission_mode: Field<String>,
    pub tool: Field<String>,
    pub problem: Option<String>,
    pub context_percent: Field<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DetailKind {
    #[default]
    Status,
    Annotation,
    Refresh,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EventTraits {
    pub kind: DetailKind,
    pub turn_start: bool,
    pub subagent_origin: bool,
    pub live_background_children: Option<u32>,
}

const MAX_DETAIL_SCALARS: usize = 120;
const ZERO_WIDTH_JOINER: char = '\u{200D}';

const fn is_format_character(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

const fn is_combining_mark(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{0483}'..='\u{0489}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FE20}'..='\u{FE2F}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

// The joiner is a format character, but stripping it would split emoji sequences apart.
pub(crate) fn sanitize(value: &str) -> String {
    let mut kept: Vec<char> = value
        .trim()
        .chars()
        .filter(|&c| !c.is_control() && (c == ZERO_WIDTH_JOINER || !is_format_character(c)))
        .take(MAX_DETAIL_SCALARS + 1)
        .collect();
    if kept.len() > MAX_DETAIL_SCALARS {
        kept.truncate(MAX_DETAIL_SCALARS);
        while kept
            .last()
            .is_some_and(|&c| c == ZERO_WIDTH_JOINER || is_combining_mark(c))
        {
            kept.pop();
        }
    }
    kept.into_iter().collect::<String>().trim().to_string()
}

pub(crate) fn text_field(value: Option<&serde_json::Value>) -> Field<String> {
    match value {
        Some(serde_json::Value::Null) => Field::Clear,
        Some(serde_json::Value::String(text)) => {
            let clean = sanitize(text);
            if clean.is_empty() {
                Field::Clear
            } else {
                Field::Set(clean)
            }
        }
        _ => Field::Keep,
    }
}

pub(crate) fn percent_field(value: Option<&serde_json::Value>, scale: f64) -> Field<u8> {
    match value {
        None => Field::Keep,
        Some(serde_json::Value::Null) => Field::Clear,
        Some(number) => number
            .as_f64()
            .filter(|n| n.is_finite())
            .map_or(Field::Keep, |n| {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "clamped to 0..=100 first"
                )]
                let percent = (n * scale).round().clamp(0.0, 100.0) as u8;
                Field::Set(percent)
            }),
    }
}

pub(crate) fn effort_field(value: Option<&serde_json::Value>) -> Field<String> {
    match value {
        Some(serde_json::Value::Object(level)) => text_field(level.get("level")),
        other => text_field(other),
    }
}

pub(crate) fn problem_token(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .map(|text| snake_case(&sanitize(text)))
        .filter(|token| !token.is_empty())
}

fn snake_case(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let mut out = String::new();
    let mut after_lower = false;
    for (index, c) in chars.iter().copied().enumerate() {
        if c.is_ascii_alphanumeric() {
            let acronym_end = c.is_ascii_uppercase()
                && index > 0
                && chars[index - 1].is_ascii_uppercase()
                && chars.get(index + 1).is_some_and(char::is_ascii_lowercase);
            if c.is_ascii_uppercase() && (after_lower || acronym_end) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            after_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        } else {
            if !out.ends_with('_') {
                out.push('_');
            }
            after_lower = false;
        }
    }
    out.trim_matches('_').to_string()
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Transition {
    pub before: Option<Phase>,
    pub after: Phase,
}

impl Transition {
    pub(crate) const fn steady(phase: Phase) -> Self {
        Self {
            before: Some(phase),
            after: phase,
        }
    }

    fn leaves_activity(self) -> bool {
        self.before.is_some_and(is_active) && !is_active(self.after)
    }
}

const fn is_active(phase: Phase) -> bool {
    matches!(phase, Phase::Working | Phase::Permission)
}

pub(crate) fn kept_session(
    current: &SessionIdentity,
    previous: Option<&SessionIdentity>,
) -> SessionIdentity {
    SessionIdentity {
        title: previous.and_then(|s| s.title.clone()),
        model: previous.and_then(|s| s.model.clone()),
        effort: previous.and_then(|s| s.effort.clone()),
        permission_mode: previous.and_then(|s| s.permission_mode.clone()),
        ..current.clone()
    }
}

pub(crate) fn merged_session(
    current: &SessionIdentity,
    previous: Option<&SessionIdentity>,
    details: &Details,
    traits: &EventTraits,
) -> SessionIdentity {
    let kept = kept_session(current, previous);
    if traits.subagent_origin {
        return kept;
    }
    let switched_model = matches!(
        (&details.model, &kept.model),
        (Field::Set(next), Some(before)) if next != before
    );
    let effort = if switched_model && !matches!(details.effort, Field::Set(_)) {
        None
    } else {
        details.effort.apply(kept.effort)
    };
    SessionIdentity {
        title: details.title.apply(kept.title),
        model: details.model.apply(kept.model),
        effort,
        permission_mode: details.permission_mode.apply(kept.permission_mode),
        ..kept
    }
}

pub(crate) fn merged_activity(
    previous: Option<&Activity>,
    details: &Details,
    traits: &EventTraits,
    own: Transition,
    aggregate: Transition,
    subagents: u32,
) -> Activity {
    let earlier = previous.cloned().unwrap_or_default();
    let (tool, problem) = if traits.turn_start {
        (None, None)
    } else {
        (earlier.tool, earlier.problem)
    };
    Activity {
        tool: if own.leaves_activity() || aggregate.leaves_activity() {
            None
        } else {
            details.tool.apply(tool)
        },
        problem: details.problem.clone().or(problem),
        subagents,
        context_percent: details.context_percent.apply(earlier.context_percent),
        turns: earlier.turns.saturating_add(u32::from(traits.turn_start)),
    }
}

pub(crate) fn child_activity(
    root: &Activity,
    details: &Details,
    aggregate: Transition,
    subagents: u32,
) -> Activity {
    Activity {
        tool: if aggregate.leaves_activity() {
            None
        } else {
            details.tool.apply(root.tool.clone())
        },
        subagents,
        ..root.clone()
    }
}

pub(crate) fn subagent_count(
    source: SubagentSource,
    children: &Children,
    previous: Option<&Activity>,
    traits: &EventTraits,
) -> u32 {
    match source {
        SubagentSource::Unreported => 0,
        SubagentSource::BackgroundTasks => traits.live_background_children.unwrap_or_else(|| {
            if traits.turn_start {
                0
            } else {
                previous.map_or(0, |activity| activity.subagents)
            }
        }),
        SubagentSource::TrackedChildren => running_children(children),
    }
}

pub(crate) fn running_children(children: &Children) -> u32 {
    let running = children
        .values()
        .filter(|phase| **phase != Phase::Idle)
        .count();
    u32::try_from(running).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_bidi_overrides_and_control_characters() {
        assert_eq!(
            sanitize("  safe\u{202E}txt.exe\nnext\t "),
            "safetxt.exenext"
        );
        assert_eq!(sanitize("a\u{200B}b\u{FEFF}c\u{0007}"), "abc");
    }

    #[test]
    fn sanitize_caps_at_120_scalars_without_a_dangling_joiner() {
        let family = "\u{1F468}\u{200D}\u{1F469}";
        let input = format!("{}{family}", "x".repeat(119));
        let capped = sanitize(&input);
        assert_eq!(capped, format!("{}\u{1F468}", "x".repeat(119)));
        assert_eq!(capped.chars().count(), 120);

        let cut_on_joiner = format!("{}{family}", "x".repeat(118));
        assert_eq!(
            sanitize(&cut_on_joiner),
            format!("{}\u{1F468}", "x".repeat(118))
        );
    }

    #[test]
    fn sanitize_keeps_text_of_exactly_120_scalars() {
        let input = format!("{}e\u{0301}", "x".repeat(118));
        assert_eq!(sanitize(&input), input);
        assert_eq!(sanitize(&"y".repeat(500)), "y".repeat(120));
    }

    #[test]
    fn sanitize_keeps_joined_emoji_and_combining_marks_inside_the_cap() {
        assert_eq!(
            sanitize("team \u{1F468}\u{200D}\u{1F469}"),
            "team \u{1F468}\u{200D}\u{1F469}"
        );
        assert_eq!(sanitize("cafe\u{0301}"), "cafe\u{0301}");
    }

    #[test]
    fn sanitize_drops_a_combining_mark_left_at_the_cap() {
        let input = format!("{}e\u{0301}\u{0301}", "x".repeat(118));
        assert_eq!(sanitize(&input), format!("{}e", "x".repeat(118)));
    }

    #[test]
    fn text_fields_distinguish_absent_null_and_values() {
        assert_eq!(text_field(None), Field::Keep);
        assert_eq!(text_field(Some(&serde_json::Value::Null)), Field::Clear);
        assert_eq!(
            text_field(Some(&serde_json::json!(" opus\n"))),
            Field::Set("opus".into())
        );
        assert_eq!(
            text_field(Some(&serde_json::json!(" \u{200B} "))),
            Field::Clear
        );
        assert_eq!(text_field(Some(&serde_json::json!(42))), Field::Keep);
    }

    #[test]
    fn percent_fields_round_and_clamp_any_number() {
        let read = |value: serde_json::Value, scale| percent_field(Some(&value), scale);
        assert_eq!(read(serde_json::json!(0.414), 100.0), Field::Set(41));
        assert_eq!(read(serde_json::json!(1.7), 100.0), Field::Set(100));
        assert_eq!(read(serde_json::json!(41.7), 1.0), Field::Set(42));
        assert_eq!(read(serde_json::json!(-3), 1.0), Field::Set(0));
        assert_eq!(read(serde_json::Value::Null, 1.0), Field::Clear);
        assert_eq!(read(serde_json::json!("41"), 1.0), Field::Keep);
        assert_eq!(percent_field(None, 1.0), Field::Keep);
    }

    #[test]
    fn problem_tokens_are_snake_cased() {
        assert_eq!(snake_case("RateLimitError"), "rate_limit_error");
        assert_eq!(snake_case("rate_limit"), "rate_limit");
        assert_eq!(snake_case("  Max-Tokens Hit "), "max_tokens_hit");
        assert_eq!(snake_case("HTTPError"), "http_error");
        assert_eq!(snake_case("APIStatusError"), "api_status_error");
        assert_eq!(snake_case("HTTP"), "http");
        assert_eq!(problem_token(Some(&serde_json::json!(" - "))), None);
        assert_eq!(problem_token(Some(&serde_json::Value::Null)), None);
    }

    fn session(model: Option<&str>, effort: Option<&str>) -> SessionIdentity {
        SessionIdentity {
            id: "a".into(),
            cwd: "/tmp".into(),
            transcript_path: None,
            title: Some("Title".into()),
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            permission_mode: Some("default".into()),
        }
    }

    fn set(value: &str) -> Field<String> {
        Field::Set(value.into())
    }

    fn root() -> EventTraits {
        EventTraits::default()
    }

    fn turn() -> EventTraits {
        EventTraits {
            turn_start: true,
            ..EventTraits::default()
        }
    }

    #[test]
    fn session_details_stick_until_an_event_sets_or_clears_them() {
        let first = merged_session(
            &session(None, None),
            Some(&session(None, None)),
            &Details {
                model: set("opus"),
                ..Details::default()
            },
            &root(),
        );
        assert_eq!(first.model.as_deref(), Some("opus"));

        let kept = merged_session(
            &session(None, None),
            Some(&first),
            &Details::default(),
            &root(),
        );
        assert_eq!(kept.model.as_deref(), Some("opus"));
        assert_eq!(kept.title.as_deref(), Some("Title"));

        let cleared = merged_session(
            &session(None, None),
            Some(&kept),
            &Details {
                title: Field::Clear,
                ..Details::default()
            },
            &root(),
        );
        assert_eq!(cleared.title, None);
        assert_eq!(cleared.model.as_deref(), Some("opus"));

        let fresh = merged_session(&session(None, None), None, &Details::default(), &root());
        assert_eq!((fresh.title, fresh.permission_mode), (None, None));
    }

    #[test]
    fn a_model_switch_clears_the_effort_unless_the_same_event_sets_it() {
        let previous = session(Some("opus"), Some("high"));
        let switch = |effort| Details {
            model: set("sonnet"),
            effort,
            ..Details::default()
        };

        let switched = merged_session(&previous, Some(&previous), &switch(Field::Keep), &root());
        assert_eq!(
            (switched.model.as_deref(), switched.effort),
            (Some("sonnet"), None)
        );

        let reset = merged_session(&previous, Some(&previous), &switch(set("low")), &root());
        assert_eq!(reset.effort.as_deref(), Some("low"));

        let same = merged_session(
            &previous,
            Some(&previous),
            &Details {
                model: set("opus"),
                ..Details::default()
            },
            &root(),
        );
        assert_eq!(same.effort.as_deref(), Some("high"));
    }

    #[test]
    fn subagent_events_keep_the_session_details_but_feed_the_tool() {
        let previous = session(Some("opus"), Some("high"));
        let details = Details {
            effort: set("low"),
            permission_mode: set("bypassPermissions"),
            tool: set("Grep"),
            ..Details::default()
        };
        let subagent = EventTraits {
            subagent_origin: true,
            ..EventTraits::default()
        };

        let merged = merged_session(&previous, Some(&previous), &details, &subagent);
        assert_eq!(merged, previous);

        let working = Transition::steady(Phase::Working);
        let activity = merged_activity(
            Some(&Activity::default()),
            &details,
            &subagent,
            working,
            working,
            0,
        );
        assert_eq!(activity.tool.as_deref(), Some("Grep"));
    }

    fn activity(tool: Option<&str>, problem: Option<&str>) -> Activity {
        Activity {
            tool: tool.map(str::to_string),
            problem: problem.map(str::to_string),
            subagents: 0,
            context_percent: Some(40),
            turns: 3,
        }
    }

    fn working_to(after: Phase) -> Transition {
        Transition {
            before: Some(Phase::Working),
            after,
        }
    }

    #[test]
    fn the_tool_follows_turns_and_the_working_phase() {
        let running = activity(Some("Bash"), None);
        let working = |details: &Details, traits: &EventTraits, after| {
            merged_activity(
                Some(&running),
                details,
                traits,
                working_to(after),
                working_to(after),
                0,
            )
            .tool
        };

        let none = Details::default();
        assert_eq!(
            working(&none, &root(), Phase::Working).as_deref(),
            Some("Bash")
        );
        assert_eq!(
            working(&none, &root(), Phase::Permission).as_deref(),
            Some("Bash")
        );
        assert_eq!(working(&none, &root(), Phase::Idle), None);
        assert_eq!(working(&none, &root(), Phase::Compacting), None);
        assert_eq!(working(&none, &turn(), Phase::Working), None);
        assert_eq!(
            working(
                &Details {
                    tool: Field::Clear,
                    ..Details::default()
                },
                &root(),
                Phase::Working
            ),
            None
        );
        assert_eq!(
            working(
                &Details {
                    tool: set("Read"),
                    ..Details::default()
                },
                &turn(),
                Phase::Working
            )
            .as_deref(),
            Some("Read")
        );
    }

    #[test]
    fn a_problem_outlasts_the_idle_phase_until_the_next_turn() {
        let failed = merged_activity(
            Some(&activity(None, None)),
            &Details {
                problem: Some("rate_limit_error".into()),
                ..Details::default()
            },
            &root(),
            working_to(Phase::Idle),
            working_to(Phase::Idle),
            0,
        );
        assert_eq!(failed.problem.as_deref(), Some("rate_limit_error"));

        let idle = Transition::steady(Phase::Idle);
        let kept = merged_activity(Some(&failed), &Details::default(), &root(), idle, idle, 0);
        assert_eq!(kept.problem.as_deref(), Some("rate_limit_error"));

        let starting = Transition {
            before: Some(Phase::Idle),
            after: Phase::Working,
        };
        let next_turn = merged_activity(
            Some(&kept),
            &Details::default(),
            &turn(),
            starting,
            starting,
            0,
        );
        assert_eq!(next_turn.problem, None);
    }

    #[test]
    fn the_context_percent_applies_its_field() {
        let previous = activity(None, None);
        let working = Transition::steady(Phase::Working);
        let percent = |context_percent| {
            merged_activity(
                Some(&previous),
                &Details {
                    context_percent,
                    ..Details::default()
                },
                &root(),
                working,
                working,
                0,
            )
            .context_percent
        };
        assert_eq!(percent(Field::Keep), Some(40));
        assert_eq!(percent(Field::Set(55)), Some(55));
        assert_eq!(percent(Field::Clear), None);
    }

    #[test]
    fn turns_count_turn_starts_only() {
        let previous = activity(None, None);
        let working = Transition::steady(Phase::Working);
        let after = |traits: &EventTraits, previous: Option<&Activity>| {
            merged_activity(previous, &Details::default(), traits, working, working, 0).turns
        };
        assert_eq!(after(&turn(), Some(&previous)), 4);
        assert_eq!(after(&root(), Some(&previous)), 3);
        assert_eq!(after(&turn(), None), 1);
        assert_eq!(after(&root(), None), 0);

        let saturated = Activity {
            turns: u32::MAX,
            ..previous
        };
        assert_eq!(after(&turn(), Some(&saturated)), u32::MAX);
    }

    #[test]
    fn subagents_count_tracked_children_or_claude_background_tasks() {
        let children = Children::from([
            ("a".to_string(), Phase::Working),
            ("b".to_string(), Phase::Idle),
            ("prov:c".to_string(), Phase::Permission),
        ]);
        let previous = Activity {
            subagents: 3,
            ..Activity::default()
        };
        let none = EventTraits::default();
        assert_eq!(
            subagent_count(SubagentSource::TrackedChildren, &children, None, &none),
            2
        );

        let tasks = SubagentSource::BackgroundTasks;
        let reported = EventTraits {
            live_background_children: Some(1),
            ..EventTraits::default()
        };
        assert_eq!(
            subagent_count(tasks, &Children::new(), Some(&previous), &reported),
            1
        );
        assert_eq!(
            subagent_count(tasks, &Children::new(), Some(&previous), &none),
            3
        );
        assert_eq!(
            subagent_count(tasks, &Children::new(), Some(&previous), &turn()),
            0
        );
        assert_eq!(subagent_count(tasks, &Children::new(), None, &none), 0);
        assert_eq!(
            subagent_count(
                SubagentSource::Unreported,
                &children,
                Some(&previous),
                &reported
            ),
            0
        );
    }

    #[test]
    fn a_child_event_feeds_the_root_tool_until_the_root_settles() {
        let root = activity(None, Some("interrupted"));
        let details = Details {
            tool: set("Bash"),
            problem: Some("overloaded".into()),
            ..Details::default()
        };
        let fed = child_activity(&root, &details, Transition::steady(Phase::Working), 1);
        assert_eq!(
            fed,
            Activity {
                tool: Some("Bash".into()),
                subagents: 1,
                ..root
            }
        );
        let settled = child_activity(&fed, &Details::default(), working_to(Phase::Idle), 0);
        assert_eq!(settled.tool, None);
    }
}
