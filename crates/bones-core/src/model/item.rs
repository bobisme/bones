use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// The three kinds of work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Task,
    Goal,
    Bug,
}

impl Kind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Goal => "goal",
            Self::Bug => "bug",
        }
    }
}

/// The four lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Open,
    Doing,
    Done,
    Archived,
}

impl State {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Doing => "doing",
            Self::Done => "done",
            Self::Archived => "archived",
        }
    }

    /// Validate whether a transition from self to `target` is allowed.
    ///
    /// Valid transitions:
    /// - `open -> doing`
    /// - `open -> done`
    /// - `doing -> done`
    /// - `doing -> open` (reopen)
    /// - `done -> archived`
    /// - `done -> open` (reopen)
    /// - `archived -> open` (reopen)
    pub fn can_transition_to(&self, target: State) -> Result<(), InvalidTransition> {
        if *self == target {
            return Err(InvalidTransition {
                from: *self,
                to: target,
                reason: "no-op transition is not allowed",
            });
        }

        let allowed = matches!(
            (*self, target),
            (Self::Open, State::Doing)
                | (Self::Open, State::Done)
                | (Self::Doing, State::Done)
                | (Self::Doing, State::Open)
                | (Self::Done, State::Archived)
                | (Self::Done, State::Open)
                | (Self::Archived, State::Open)
        );

        if allowed {
            Ok(())
        } else {
            Err(InvalidTransition {
                from: *self,
                to: target,
                reason: "transition not allowed by lifecycle rules",
            })
        }
    }
}

/// Human override for computed priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    Urgent,
    Default,
    Punt,
}

impl Default for Urgency {
    fn default() -> Self {
        Self::Default
    }
}

impl Urgency {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Urgent => "urgent",
            Self::Default => "default",
            Self::Punt => "punt",
        }
    }
}

/// Optional t-shirt sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Size {
    Xxs,
    Xs,
    S,
    M,
    L,
    Xl,
    Xxl,
}

impl Size {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Xxs => "xxs",
            Self::Xs => "xs",
            Self::S => "s",
            Self::M => "m",
            Self::L => "l",
            Self::Xl => "xl",
            Self::Xxl => "xxl",
        }
    }
}

/// All persisted fields for a work item (the projection-level aggregate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkItemFields {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub kind: Kind,
    pub state: State,
    pub urgency: Urgency,
    pub size: Option<Size>,
    pub parent_id: Option<String>,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
    pub blocked_by: Vec<String>,
    pub related_to: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Default for WorkItemFields {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            description: None,
            kind: Kind::Task,
            state: State::Open,
            urgency: Urgency::Default,
            size: None,
            parent_id: None,
            assignees: Vec::new(),
            labels: Vec::new(),
            blocked_by: Vec::new(),
            related_to: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }
}

/// Error returned when a state transition is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    pub from: State,
    pub to: State,
    pub reason: &'static str,
}

/// Error returned when parsing an enum value from text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseEnumError {
    pub expected: &'static str,
    pub got: String,
}

impl fmt::Display for ParseEnumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: '{}'", self.expected, self.got)
    }
}

impl std::error::Error for ParseEnumError {}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for Urgency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn normalize(input: &str) -> String {
    input.trim().to_ascii_lowercase()
}

impl FromStr for Kind {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = normalize(s);
        match normalized.as_str() {
            "task" => Ok(Self::Task),
            "goal" => Ok(Self::Goal),
            "bug" => Ok(Self::Bug),
            _ => Err(ParseEnumError {
                expected: "kind",
                got: s.to_string(),
            }),
        }
    }
}

impl FromStr for State {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = normalize(s);
        match normalized.as_str() {
            "open" => Ok(Self::Open),
            "doing" => Ok(Self::Doing),
            "done" => Ok(Self::Done),
            "archived" => Ok(Self::Archived),
            _ => Err(ParseEnumError {
                expected: "state",
                got: s.to_string(),
            }),
        }
    }
}

impl FromStr for Urgency {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = normalize(s);
        match normalized.as_str() {
            "urgent" => Ok(Self::Urgent),
            "default" => Ok(Self::Default),
            "punt" => Ok(Self::Punt),
            _ => Err(ParseEnumError {
                expected: "urgency",
                got: s.to_string(),
            }),
        }
    }
}

impl FromStr for Size {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = normalize(s);
        match normalized.as_str() {
            "xxs" => Ok(Self::Xxs),
            "xs" => Ok(Self::Xs),
            "s" => Ok(Self::S),
            "m" => Ok(Self::M),
            "l" => Ok(Self::L),
            "xl" => Ok(Self::Xl),
            "xxl" => Ok(Self::Xxl),
            _ => Err(ParseEnumError {
                expected: "size",
                got: s.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InvalidTransition, Kind, Size, State, Urgency, WorkItemFields};
    use std::str::FromStr;

    #[test]
    fn enum_json_roundtrips() {
        assert_eq!(serde_json::to_string(&Kind::Task).unwrap(), "\"task\"");
        assert_eq!(serde_json::to_string(&State::Doing).unwrap(), "\"doing\"");
        assert_eq!(
            serde_json::to_string(&Urgency::Default).unwrap(),
            "\"default\""
        );
        assert_eq!(serde_json::to_string(&Size::Xxl).unwrap(), "\"xxl\"");

        assert_eq!(serde_json::from_str::<Kind>("\"bug\"").unwrap(), Kind::Bug);
        assert_eq!(
            serde_json::from_str::<State>("\"open\"").unwrap(),
            State::Open
        );
        assert_eq!(
            serde_json::from_str::<Urgency>("\"urgent\"").unwrap(),
            Urgency::Urgent
        );
        assert_eq!(serde_json::from_str::<Size>("\"xs\"").unwrap(), Size::Xs);
    }

    #[test]
    fn display_parse_roundtrips() {
        for value in [Kind::Task, Kind::Goal, Kind::Bug] {
            let rendered = value.to_string();
            let reparsed = Kind::from_str(&rendered).unwrap();
            assert_eq!(value, reparsed);
        }

        for value in [State::Open, State::Doing, State::Done, State::Archived] {
            let rendered = value.to_string();
            let reparsed = State::from_str(&rendered).unwrap();
            assert_eq!(value, reparsed);
        }

        for value in [Urgency::Urgent, Urgency::Default, Urgency::Punt] {
            let rendered = value.to_string();
            let reparsed = Urgency::from_str(&rendered).unwrap();
            assert_eq!(value, reparsed);
        }

        for value in [
            Size::Xxs,
            Size::Xs,
            Size::S,
            Size::M,
            Size::L,
            Size::Xl,
            Size::Xxl,
        ] {
            let rendered = value.to_string();
            let reparsed = Size::from_str(&rendered).unwrap();
            assert_eq!(value, reparsed);
        }
    }

    #[test]
    fn parse_rejects_unknown_values() {
        assert!(Kind::from_str("epic").is_err());
        assert!(State::from_str("active").is_err());
        assert!(Urgency::from_str("hot").is_err());
        assert!(Size::from_str("mega").is_err());
    }

    #[test]
    fn state_transition_rules() {
        assert!(State::Open.can_transition_to(State::Doing).is_ok());
        assert!(State::Open.can_transition_to(State::Done).is_ok());
        assert!(State::Doing.can_transition_to(State::Done).is_ok());
        assert!(State::Doing.can_transition_to(State::Open).is_ok());
        assert!(State::Done.can_transition_to(State::Archived).is_ok());
        assert!(State::Done.can_transition_to(State::Open).is_ok());
        assert!(State::Archived.can_transition_to(State::Open).is_ok());

        assert!(matches!(
            State::Open.can_transition_to(State::Archived),
            Err(InvalidTransition {
                from: State::Open,
                to: State::Archived,
                ..
            })
        ));

        assert!(matches!(
            State::Done.can_transition_to(State::Doing),
            Err(InvalidTransition {
                from: State::Done,
                to: State::Doing,
                ..
            })
        ));
    }

    #[test]
    fn work_item_fields_default_is_stable() {
        let fields = WorkItemFields::default();
        assert_eq!(fields.id, "");
        assert_eq!(fields.title, "");
        assert_eq!(fields.kind, Kind::Task);
        assert_eq!(fields.state, State::Open);
        assert_eq!(fields.urgency, Urgency::Default);
        assert!(fields.description.is_none());
        assert!(fields.size.is_none());
        assert!(fields.parent_id.is_none());
        assert!(fields.assignees.is_empty());
        assert!(fields.labels.is_empty());
        assert!(fields.blocked_by.is_empty());
        assert!(fields.related_to.is_empty());
        assert_eq!(fields.created_at, 0);
        assert_eq!(fields.updated_at, 0);
    }
}
