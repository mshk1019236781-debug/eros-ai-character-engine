// SPDX-License-Identifier: AGPL-3.0-only
//! Stable world/entity facts — the layer that survives after the sentence which
//! stated them has left the recent raw history.
//!
//! ```text
//! "白远舟是白芷的哥哥"   → world fact   (stable, reusable, no story stage)
//! "白远舟昨天来医院接白芷" → event       (recent_episodes, has a stage)
//! ```
//!
//! The split is deliberate and one-directional: a world fact never becomes an
//! event, never gets an embedding and never gets a graph edge. Events stay the
//! episodic layer with their own scorer, edges and cooldown; facts are a small,
//! deduplicated lookup table.
//!
//! Nothing here calls a model. Candidates arrive from the main RP completion
//! (the same `<eros_memory>` trailer that already carries the event), which is
//! what keeps the "no new LLM call" property true.

use serde::{Deserialize, Serialize};

/// Longest accepted `subject` / `predicate` / `object`.
pub const MAX_WORLD_FACT_FIELD_CHARS: usize = 60;
/// Longest accepted natural-language `statement`.
pub const MAX_WORLD_FACT_STATEMENT_CHARS: usize = 160;
pub const MAX_WORLD_FACT_KNOWLEDGE_SCOPE: usize = 8;
/// Facts one completion may contribute. A trailer with more is truncated rather
/// than rejected: the event beside it is still worth keeping.
pub const MAX_WORLD_FACTS_PER_METADATA: usize = 8;
/// Prompt budget. Facts are background, not a dossier — the whole table must
/// never reach the model.
pub const DEFAULT_MAX_WORLD_FACTS_IN_PROMPT: usize = 6;
/// How many active facts the retrieval step will even look at.
pub const WORLD_FACT_SCAN_LIMIT: i64 = 100;

/// The closed vocabulary of stable fact kinds. Anything else — an action, a
/// mood, a one-off NPC, today's outfit — is not a world fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactType {
    Identity,
    Relationship,
    Occupation,
    Affiliation,
    WorldSetting,
}

impl FactType {
    pub const ALL: [FactType; 5] = [
        FactType::Identity,
        FactType::Relationship,
        FactType::Occupation,
        FactType::Affiliation,
        FactType::WorldSetting,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            FactType::Identity => "identity",
            FactType::Relationship => "relationship",
            FactType::Occupation => "occupation",
            FactType::Affiliation => "affiliation",
            FactType::WorldSetting => "world_setting",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "identity" => Some(FactType::Identity),
            "relationship" => Some(FactType::Relationship),
            "occupation" => Some(FactType::Occupation),
            "affiliation" => Some(FactType::Affiliation),
            "world_setting" => Some(FactType::WorldSetting),
            _ => None,
        }
    }

    /// True when both ends of the triple are people, so the object side must be
    /// a concrete name rather than a role noun.
    pub fn is_person_to_person(self) -> bool {
        matches!(self, FactType::Identity | FactType::Relationship)
    }
}

/// One candidate as the main RP model writes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldFactCandidate {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    #[serde(rename = "type")]
    pub fact_type: FactType,
    /// Natural one-line rendering. Optional so a model that omits it still
    /// stores a usable fact; the deterministic fallback below is never wrong.
    #[serde(default)]
    pub statement: Option<String>,
    /// Characters allowed to see this fact. Empty = everyone in this story.
    #[serde(default)]
    pub knowledge_scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldFactError {
    EmptySubject,
    EmptyPredicate,
    EmptyObject,
    FieldTooLong {
        field: &'static str,
        limit: usize,
    },
    StatementTooLong {
        limit: usize,
    },
    TooManyKnowledgeScopes {
        limit: usize,
    },
    EmptyKnowledgeScope {
        index: usize,
    },
    /// The subject is a role noun ("服务员"), not an entity. A transient extra
    /// must not become a permanent fact just because it was mentioned.
    GenericSubject {
        subject: String,
    },
    GenericObject {
        object: String,
    },
}

impl std::fmt::Display for WorldFactError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorldFactError::EmptySubject => write!(formatter, "subject must not be empty"),
            WorldFactError::EmptyPredicate => write!(formatter, "predicate must not be empty"),
            WorldFactError::EmptyObject => write!(formatter, "object must not be empty"),
            WorldFactError::FieldTooLong { field, limit } => {
                write!(formatter, "{field} exceeds {limit} characters")
            }
            WorldFactError::StatementTooLong { limit } => {
                write!(formatter, "statement exceeds {limit} characters")
            }
            WorldFactError::TooManyKnowledgeScopes { limit } => {
                write!(formatter, "more than {limit} knowledge scopes")
            }
            WorldFactError::EmptyKnowledgeScope { index } => {
                write!(formatter, "knowledge_scope[{index}] is blank")
            }
            WorldFactError::GenericSubject { subject } => {
                write!(
                    formatter,
                    "subject `{subject}` is a transient role, not an entity"
                )
            }
            WorldFactError::GenericObject { object } => {
                write!(
                    formatter,
                    "object `{object}` is a transient role, not an entity"
                )
            }
        }
    }
}

impl std::error::Error for WorldFactError {}

impl WorldFactCandidate {
    /// Trim every scalar, collapse inner whitespace and de-duplicate the scope.
    pub fn normalize(&mut self) {
        self.subject = normalize_name(&self.subject);
        self.predicate = normalize_name(&self.predicate);
        self.object = normalize_name(&self.object);
        self.statement = self
            .statement
            .as_ref()
            .map(|statement| normalize_name(statement))
            .filter(|statement| !statement.is_empty());
        self.knowledge_scope = normalize_names(&self.knowledge_scope);
    }

    pub fn validate(&self) -> Result<(), WorldFactError> {
        check_field("subject", &self.subject, WorldFactError::EmptySubject)?;
        check_field("predicate", &self.predicate, WorldFactError::EmptyPredicate)?;
        check_field("object", &self.object, WorldFactError::EmptyObject)?;
        if let Some(statement) = self.statement.as_ref() {
            if statement.chars().count() > MAX_WORLD_FACT_STATEMENT_CHARS {
                return Err(WorldFactError::StatementTooLong {
                    limit: MAX_WORLD_FACT_STATEMENT_CHARS,
                });
            }
        }
        if self.knowledge_scope.len() > MAX_WORLD_FACT_KNOWLEDGE_SCOPE {
            return Err(WorldFactError::TooManyKnowledgeScopes {
                limit: MAX_WORLD_FACT_KNOWLEDGE_SCOPE,
            });
        }
        if let Some(index) = self
            .knowledge_scope
            .iter()
            .position(|viewer| viewer.trim().is_empty())
        {
            return Err(WorldFactError::EmptyKnowledgeScope { index });
        }
        if is_transient_role(&self.subject) {
            return Err(WorldFactError::GenericSubject {
                subject: self.subject.clone(),
            });
        }
        if self.fact_type.is_person_to_person() && is_transient_role(&self.object) {
            return Err(WorldFactError::GenericObject {
                object: self.object.clone(),
            });
        }
        Ok(())
    }

    /// The line that reaches the prompt.
    pub fn statement_text(&self) -> String {
        match self.statement.as_ref() {
            Some(statement) if !statement.trim().is_empty() => statement.trim().to_string(),
            // Readable, never wrong: a labelled triple instead of a sentence
            // whose grammar this layer would have to guess at.
            _ => format!(
                "{}：{}＝{}",
                self.subject.trim(),
                self.predicate.trim(),
                self.object.trim()
            ),
        }
    }
}

fn check_field(
    field: &'static str,
    value: &str,
    empty: WorldFactError,
) -> Result<(), WorldFactError> {
    if value.trim().is_empty() {
        return Err(empty);
    }
    if value.chars().count() > MAX_WORLD_FACT_FIELD_CHARS {
        return Err(WorldFactError::FieldTooLong {
            field,
            limit: MAX_WORLD_FACT_FIELD_CHARS,
        });
    }
    Ok(())
}

/// Trim and collapse runs of whitespace; Chinese names have no case, so this is
/// the whole normalisation for a name.
pub fn normalize_name(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_names(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        let trimmed = normalize_name(value);
        if trimmed.is_empty() {
            continue;
        }
        if !out.iter().any(|seen| seen == &trimmed) {
            out.push(trimmed);
        }
    }
    out
}

/// Role nouns that describe a function in one scene, never an entity.
///
/// This is the program-side half of requirement B: the model is told not to
/// promote a waiter to a permanent character, and this list is what makes that
/// instruction enforceable rather than aspirational.
pub const TRANSIENT_ROLE_NOUNS: [&str; 26] = [
    "服务员",
    "店员",
    "老板",
    "路人",
    "陌生人",
    "某人",
    "一个人",
    "那个人",
    "那人",
    "这人",
    "顾客",
    "客人",
    "司机",
    "保安",
    "前台",
    "邻居",
    "同事",
    "同学",
    "朋友",
    "老师",
    "医生",
    "护士",
    "警察",
    "助理",
    "男人",
    "女人",
];

/// True when `name` is a bare role noun rather than a concrete entity.
pub fn is_transient_role(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    TRANSIENT_ROLE_NOUNS
        .iter()
        .any(|role| role == &name || name == format!("一个{role}") || name == format!("那位{role}"))
}

/// Is one stored fact worth spending prompt budget on this turn?
///
/// A fact earns its place by naming someone in the current scene (the POV
/// character or a participant) or by being mentioned in what the user just
/// said. Everything else stays in the table: the requirement is that the whole
/// world-fact store can never be dumped into a prompt.
pub fn world_fact_is_relevant(
    subject: &str,
    object: &str,
    viewer: Option<&str>,
    participants: &[String],
    query_text: &str,
) -> bool {
    let subject = subject.trim();
    let object = object.trim();
    let names_viewer = |name: &str| {
        !name.is_empty()
            && (viewer.is_some_and(|viewer| viewer.trim() == name)
                || participants
                    .iter()
                    .any(|participant| participant.trim() == name))
    };
    if names_viewer(subject) || names_viewer(object) {
        return true;
    }
    (!subject.is_empty() && query_text.contains(subject))
        || (!object.is_empty() && query_text.contains(object))
}

/// Render the prompt section. Empty input renders nothing, so a turn with no
/// relevant fact leaves the prompt byte-identical.
pub fn render_world_facts(statements: &[String]) -> Option<String> {
    let lines: Vec<String> = statements
        .iter()
        .map(|statement| statement.trim())
        .filter(|statement| !statement.is_empty())
        .map(|statement| format!("- {statement}"))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!("[world_facts]\n{}", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(json: &str) -> WorldFactCandidate {
        serde_json::from_str(json).expect("candidate parses")
    }

    fn valid(
        subject: &str,
        predicate: &str,
        object: &str,
        fact_type: FactType,
    ) -> WorldFactCandidate {
        let mut candidate = WorldFactCandidate {
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            fact_type,
            statement: None,
            knowledge_scope: Vec::new(),
        };
        candidate.normalize();
        candidate
    }

    #[test]
    fn parses_the_documented_shape() {
        let fact = parsed(
            r#"{"subject":"白远舟","predicate":"的哥哥","object":"白芷",
                "type":"relationship","statement":"白远舟是白芷的哥哥",
                "knowledge_scope":[]}"#,
        );
        assert_eq!(fact.fact_type, FactType::Relationship);
        assert_eq!(fact.statement_text(), "白远舟是白芷的哥哥");
    }

    #[test]
    fn unknown_fact_type_is_rejected() {
        let raw = r#"{"subject":"a","predicate":"b","object":"c","type":"mood"}"#;
        assert!(serde_json::from_str::<WorldFactCandidate>(raw).is_err());
    }

    #[test]
    fn unknown_field_is_rejected() {
        let raw = r#"{"subject":"a","predicate":"b","object":"c","type":"identity","hint":"x"}"#;
        assert!(serde_json::from_str::<WorldFactCandidate>(raw).is_err());
    }

    #[test]
    fn missing_statement_falls_back_to_the_labelled_triple() {
        let fact = valid("白远舟", "的哥哥", "白芷", FactType::Relationship);
        assert_eq!(fact.statement_text(), "白远舟：的哥哥＝白芷");
    }

    #[test]
    fn blank_fields_are_rejected() {
        assert_eq!(
            valid("  ", "b", "c", FactType::Identity).validate(),
            Err(WorldFactError::EmptySubject)
        );
        assert_eq!(
            valid("a", "", "c", FactType::Identity).validate(),
            Err(WorldFactError::EmptyPredicate)
        );
        assert_eq!(
            valid("a", "b", " ", FactType::Identity).validate(),
            Err(WorldFactError::EmptyObject)
        );
    }

    #[test]
    fn overlong_fields_are_rejected() {
        let long = "字".repeat(MAX_WORLD_FACT_FIELD_CHARS + 1);
        assert!(matches!(
            valid(&long, "b", "c", FactType::Identity).validate(),
            Err(WorldFactError::FieldTooLong {
                field: "subject",
                ..
            })
        ));
        let mut fact = valid("a", "b", "c", FactType::Identity);
        fact.statement = Some("字".repeat(MAX_WORLD_FACT_STATEMENT_CHARS + 1));
        assert!(matches!(
            fact.validate(),
            Err(WorldFactError::StatementTooLong { .. })
        ));
    }

    /// Requirement B: a transient extra must not become a permanent entity.
    #[test]
    fn a_waiter_is_not_an_entity() {
        assert_eq!(
            valid("服务员", "倒了", "水", FactType::Identity).validate(),
            Err(WorldFactError::GenericSubject {
                subject: "服务员".to_string()
            })
        );
        assert_eq!(
            valid("一个店员", "说", "欢迎光临", FactType::Affiliation).validate(),
            Err(WorldFactError::GenericSubject {
                subject: "一个店员".to_string()
            })
        );
    }

    #[test]
    fn a_named_character_is_an_entity() {
        assert!(valid("白远舟", "的哥哥", "白芷", FactType::Relationship)
            .validate()
            .is_ok());
    }

    #[test]
    fn occupation_may_point_at_a_role_noun() {
        // The subject is the named character; the object is the job itself, so
        // the role-noun rule must not fire on the object side here.
        assert!(valid("温景行", "是", "医生", FactType::Occupation)
            .validate()
            .is_ok());
        // …but a person-to-person fact may not name a role as its object.
        assert_eq!(
            valid("白远舟", "的哥哥", "服务员", FactType::Relationship).validate(),
            Err(WorldFactError::GenericObject {
                object: "服务员".to_string()
            })
        );
    }

    #[test]
    fn scope_list_is_normalised_and_capped() {
        let mut fact = valid("a", "b", "c", FactType::Identity);
        fact.knowledge_scope = vec![" 白芷 ".into(), "白芷".into(), "".into()];
        fact.normalize();
        assert_eq!(fact.knowledge_scope, vec!["白芷".to_string()]);
        fact.knowledge_scope = (0..=MAX_WORLD_FACT_KNOWLEDGE_SCOPE)
            .map(|i| format!("c{i}"))
            .collect();
        assert!(matches!(
            fact.validate(),
            Err(WorldFactError::TooManyKnowledgeScopes { .. })
        ));
    }

    #[test]
    fn relevance_needs_a_name_in_scene_or_in_the_turn() {
        let participants = vec!["裴烬".to_string()];
        // Named in the current user text.
        assert!(world_fact_is_relevant(
            "白远舟",
            "白芷",
            Some("裴烬"),
            &participants,
            "白远舟最近怎么样？"
        ));
        // The POV character is the object.
        assert!(world_fact_is_relevant(
            "温景行",
            "裴烬",
            Some("裴烬"),
            &participants,
            "今天天气不错"
        ));
        // Unrelated to the scene and unmentioned.
        assert!(!world_fact_is_relevant(
            "白远舟",
            "白芷",
            Some("裴烬"),
            &participants,
            "今天天气不错"
        ));
    }

    #[test]
    fn rendering_is_absent_when_there_is_nothing_to_inject() {
        assert_eq!(render_world_facts(&[]), None);
        assert_eq!(render_world_facts(&["  ".to_string()]), None);
        assert_eq!(
            render_world_facts(&["白远舟是白芷的哥哥".to_string()]),
            Some("[world_facts]\n- 白远舟是白芷的哥哥".to_string())
        );
    }
}
