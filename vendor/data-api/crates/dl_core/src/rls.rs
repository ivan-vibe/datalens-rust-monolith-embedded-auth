//! Row-level security (RLS) rules of a dataset.
//!
//! The stored `us` dataset keeps a flat list of [`RlsEntry`] rows: a field, an allowed value
//! (or `userid`/`all` pattern) and a subject (`user`, `group`, `all` or `userid`). The query
//! translator turns the restrictions for the current user into anonymous `IN` filters — the
//! same place and shape as Python's `dl_rls` + `QueryFormalizer._make_rls_filter_specs`.
//!
//! The `rls` API field can also be a map `{field_guid: "<text config>"}`; that legacy spelling
//! is parsed here, so a stored dataset and an inline one are handled the same way.

use std::collections::{BTreeMap, HashSet};

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

/// `dl_constants.RLSPatternType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RlsPattern {
    #[default]
    Value,
    All,
    Userid,
}

/// `dl_constants.RLSSubjectType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RlsSubjectType {
    User,
    Group,
    All,
    Userid,
    #[default]
    Unknown,
    NotFound,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RlsSubject {
    #[serde(default)]
    pub subject_type: RlsSubjectType,
    #[serde(default)]
    pub subject_id: String,
    #[serde(default)]
    pub subject_name: Option<String>,
}

/// One stored RLS rule (`dl_rls.models.RLSEntry`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RlsEntry {
    #[serde(default)]
    pub field_guid: String,
    #[serde(default)]
    pub allowed_value: Option<String>,
    #[serde(default)]
    pub pattern_type: RlsPattern,
    #[serde(default)]
    pub subject: RlsSubject,
}

/// The RLS rules of one dataset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Rls {
    pub entries: Vec<RlsEntry>,
}

impl Rls {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add entries addressed by a `rls2` map (`{field_guid: [entries]}`).
    ///
    /// `rls2` is the editor-facing shape: the field is the map key and is usually absent from
    /// each entry. The stored `rls` shape carries it on every entry.
    pub fn extend_rls2(&mut self, map: &BTreeMap<String, Vec<RlsEntry>>) {
        for (field_guid, entries) in map {
            for entry in entries {
                let mut entry = entry.clone();
                if entry.field_guid.is_empty() {
                    entry.field_guid = field_guid.clone();
                }
                self.entries.push(entry);
            }
        }
    }

    /// The restrictions for one user, by field guid.
    ///
    /// This is the algorithm of `dl_rls.RLS.get_user_restrictions`:
    ///
    /// * a matching `all` entry removes the restriction from the field;
    /// * `user` entries match by id, `group` entries by membership, `all` matches every user;
    /// * a `userid` entry adds the current user id to the allowed list;
    /// * a field with rules but no matching entries becomes `IN ()` (no rows), not
    ///   unrestricted.
    pub fn restrictions(&self, user_id: &str, groups: &[String]) -> BTreeMap<String, Vec<String>> {
        if self.entries.is_empty() {
            return BTreeMap::new();
        }
        let groups: HashSet<&str> = groups.iter().map(String::as_str).collect();
        let mut by_field: BTreeMap<&str, Vec<&RlsEntry>> = BTreeMap::new();
        for entry in &self.entries {
            by_field
                .entry(entry.field_guid.as_str())
                .or_default()
                .push(entry);
        }

        let mut result = BTreeMap::new();
        for (field_guid, entries) in by_field {
            // Same as `get_entries`: an entry matches the user if its subject does, or if it is
            // the `userid` pattern (which is checked below).
            let matching: Vec<&RlsEntry> = entries
                .iter()
                .copied()
                .filter(|entry| match entry.subject.subject_type {
                    RlsSubjectType::User => entry.subject.subject_id == user_id,
                    RlsSubjectType::Group => groups.contains(entry.subject.subject_id.as_str()),
                    RlsSubjectType::All => true,
                    RlsSubjectType::Userid => entry.pattern_type == RlsPattern::Userid,
                    RlsSubjectType::Unknown | RlsSubjectType::NotFound => false,
                })
                .filter(|entry| {
                    // `userid` is meaningful only as the `userid: userid` line.
                    entry.pattern_type != RlsPattern::Userid
                        || entry.subject.subject_type == RlsSubjectType::Userid
                })
                .collect();

            if matching
                .iter()
                .any(|entry| entry.pattern_type == RlsPattern::All)
            {
                // `*: <subject>` — this subject sees the whole field.
                continue;
            }

            let mut allowed_values = Vec::new();
            let mut allow_userid = false;
            for entry in &matching {
                match entry.pattern_type {
                    RlsPattern::Value => {
                        if let Some(value) = &entry.allowed_value {
                            allowed_values.push(value.clone());
                        }
                    }
                    RlsPattern::Userid => allow_userid = true,
                    RlsPattern::All => {}
                }
            }
            if allow_userid {
                allowed_values.push(user_id.to_string());
            }
            allowed_values.sort();
            allowed_values.dedup();
            result.insert(field_guid.to_string(), allowed_values);
        }
        result
    }
}

impl<'de> Deserialize<'de> for Rls {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Null => Ok(Self::default()),
            serde_json::Value::Array(items) => {
                let entries = serde_json::from_value(serde_json::Value::Array(items))
                    .map_err(de::Error::custom)?;
                Ok(Self { entries })
            }
            serde_json::Value::Object(map) => {
                let mut entries = Vec::new();
                for (field_guid, config) in map {
                    match config {
                        serde_json::Value::String(text) => {
                            entries.extend(
                                parse_text_config(&field_guid, &text).map_err(de::Error::custom)?,
                            );
                        }
                        serde_json::Value::Array(items) => {
                            let mut parsed: Vec<RlsEntry> =
                                serde_json::from_value(serde_json::Value::Array(items))
                                    .map_err(de::Error::custom)?;
                            for entry in &mut parsed {
                                if entry.field_guid.is_empty() {
                                    entry.field_guid = field_guid.clone();
                                }
                            }
                            entries.extend(parsed);
                        }
                        _ => {
                            return Err(de::Error::custom(format!(
                                "RLS for field {field_guid} must be a string or a list"
                            )))
                        }
                    }
                }
                Ok(Self { entries })
            }
            _ => Err(de::Error::custom("RLS must be a list or an object")),
        }
    }
}

/// Parse a legacy text RLS config (`'value': user1, @group:g` / `*: user` / `userid: userid`).
fn parse_text_config(field_guid: &str, config: &str) -> Result<Vec<RlsEntry>, String> {
    let mut entries = Vec::new();
    for (line_no, raw_line) in config.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let (pattern, value, subjects) = if let Some(rest) = line.strip_prefix("*:") {
            (RlsPattern::All, None, rest.trim())
        } else if line == "userid: userid" {
            entries.push(RlsEntry {
                field_guid: field_guid.to_string(),
                pattern_type: RlsPattern::Userid,
                subject: RlsSubject {
                    subject_type: RlsSubjectType::Userid,
                    subject_id: String::new(),
                    subject_name: Some("userid".to_string()),
                },
                ..RlsEntry::default()
            });
            continue;
        } else if line.starts_with('\'') {
            let (value, rest) = split_quoted(line)
                .ok_or_else(|| format!("RLS line {}: unterminated quoted value", line_no + 1))?;
            let rest = rest.trim_start();
            let rest = rest.strip_prefix(':').ok_or_else(|| {
                format!(
                    "RLS line {}: ':' expected after the quoted value",
                    line_no + 1
                )
            })?;
            (RlsPattern::Value, Some(value), rest.trim())
        } else {
            return Err(format!("RLS line {}: wrong format", line_no + 1));
        };

        for name in subjects
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            entries.push(RlsEntry {
                field_guid: field_guid.to_string(),
                allowed_value: value.clone(),
                pattern_type: pattern,
                subject: subject_named(name),
            });
        }
    }
    Ok(entries)
}

/// Split `'value': rest` respecting SQL-style doubled quotes (`''`).
fn split_quoted(line: &str) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut chars = line.char_indices().skip(1);
    while let Some((index, ch)) = chars.next() {
        if ch == '\'' {
            // The next char is either the closing quote or another quote (an escaped one).
            if let Some((_, '\'')) = chars.clone().next() {
                value.push('\'');
                chars.next();
                continue;
            }
            return Some((value, &line[index + 1..]));
        }
        value.push(ch);
    }
    None
}

fn subject_named(name: &str) -> RlsSubject {
    match name {
        "*" => RlsSubject {
            subject_type: RlsSubjectType::All,
            subject_id: "*".to_string(),
            subject_name: Some("*".to_string()),
        },
        _ if name.starts_with("@group:") => RlsSubject {
            subject_type: RlsSubjectType::Group,
            subject_id: name.trim_start_matches("@group:").to_string(),
            subject_name: Some(name.to_string()),
        },
        _ if name.starts_with("@sa:") => RlsSubject {
            subject_type: RlsSubjectType::User,
            subject_id: name.trim_start_matches("@sa:").to_string(),
            subject_name: Some(name.to_string()),
        },
        _ => RlsSubject {
            subject_type: RlsSubjectType::User,
            subject_id: name.to_string(),
            subject_name: Some(name.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        field: &str,
        value: Option<&str>,
        pattern: RlsPattern,
        subject: RlsSubject,
    ) -> RlsEntry {
        RlsEntry {
            field_guid: field.to_string(),
            allowed_value: value.map(str::to_string),
            pattern_type: pattern,
            subject,
        }
    }

    fn user(id: &str) -> RlsSubject {
        RlsSubject {
            subject_type: RlsSubjectType::User,
            subject_id: id.to_string(),
            subject_name: Some(id.to_string()),
        }
    }

    #[test]
    fn a_user_sees_only_the_values_configured_for_them() {
        let rls = Rls {
            entries: vec![
                entry("region", Some("Europe"), RlsPattern::Value, user("u1")),
                entry("region", Some("Asia"), RlsPattern::Value, user("u2")),
                entry(
                    "region",
                    Some("Asia"),
                    RlsPattern::Value,
                    RlsSubject {
                        subject_type: RlsSubjectType::All,
                        subject_id: "*".into(),
                        subject_name: Some("*".into()),
                    },
                ),
            ],
        };
        assert_eq!(
            rls.restrictions("u1", &[])["region"],
            vec!["Asia", "Europe"]
        );
        assert_eq!(rls.restrictions("u2", &[])["region"], vec!["Asia"]);
    }

    #[test]
    fn a_userid_rule_adds_the_user_id_and_an_all_rule_removes_the_restriction() {
        let userid = Rls {
            entries: vec![entry(
                "owner",
                None,
                RlsPattern::Userid,
                RlsSubject {
                    subject_type: RlsSubjectType::Userid,
                    subject_id: String::new(),
                    subject_name: Some("userid".into()),
                },
            )],
        };
        assert_eq!(userid.restrictions("ivan", &[])["owner"], vec!["ivan"]);

        let all = Rls {
            entries: vec![entry("owner", None, RlsPattern::All, user("ivan"))],
        };
        assert!(!all.restrictions("ivan", &[]).contains_key("owner"));
    }

    #[test]
    fn no_matching_rule_means_no_rows() {
        let rls = Rls {
            entries: vec![entry(
                "region",
                Some("Europe"),
                RlsPattern::Value,
                user("u1"),
            )],
        };
        assert!(rls.restrictions("u2", &[])["region"].is_empty());
    }

    #[test]
    fn a_group_rule_matches_membership() {
        let rls = Rls {
            entries: vec![entry(
                "region",
                Some("Europe"),
                RlsPattern::Value,
                RlsSubject {
                    subject_type: RlsSubjectType::Group,
                    subject_id: "eu".into(),
                    subject_name: Some("@group:eu".into()),
                },
            )],
        };
        assert_eq!(
            rls.restrictions("u1", &["eu".to_string()])["region"],
            vec!["Europe"]
        );
        assert!(rls.restrictions("u1", &[])["region"].is_empty());
    }

    #[test]
    fn the_legacy_text_config_is_parsed() {
        let value = serde_json::json!({
            "region": "'Europe': user1, @group:eu\n*: user2\nuserid: userid"
        });
        let rls: Rls = serde_json::from_value(value).unwrap();
        let u1 = rls.restrictions("user1", &[]);
        assert_eq!(u1["region"], vec!["Europe", "user1"]);
        assert!(!rls.restrictions("user2", &[]).contains_key("region"));
    }

    #[test]
    fn rls2_entries_get_the_field_from_the_map() {
        let mut rls = Rls::default();
        let mut map = BTreeMap::new();
        map.insert(
            "region".to_string(),
            vec![entry("", Some("Europe"), RlsPattern::Value, user("u1"))],
        );
        rls.extend_rls2(&map);
        assert_eq!(rls.entries[0].field_guid, "region");
    }
}
