//! Pure snapshot, element-reference, and semantic-diff state for agent sessions.

#![deny(clippy::unwrap_used, clippy::expect_used)]
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::num::NonZeroUsize;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ElementRef(u64);

impl ElementRef {
    pub fn number(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ElementRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "e{}", self.0)
    }
}

impl FromStr for ElementRef {
    type Err = InvalidElementRef;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let number = value
            .strip_prefix('e')
            .and_then(|number| number.parse::<u64>().ok())
            .filter(|number| *number != 0)
            .ok_or_else(|| InvalidElementRef(value.to_owned()))?;
        Ok(Self(number))
    }
}

impl Serialize for ElementRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ElementRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid element reference {0:?}; expected eN where N is positive")]
pub struct InvalidElementRef(String);

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct LocatorIdentity(pub String);

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Scroll {
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub document_width: f64,
    #[serde(default)]
    pub document_height: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressed: Option<bool>,
}

impl SemanticState {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SemanticNode {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    #[serde(
        default,
        rename = "states",
        skip_serializing_if = "SemanticState::is_empty"
    )]
    pub state: SemanticState,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SemanticElement {
    #[serde(rename = "ref")]
    pub element_ref: ElementRef,
    #[serde(skip_serializing)]
    pub identity: LocatorIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<ElementRef>,
    #[serde(flatten)]
    pub node: SemanticNode,
}

/// Integration input produced by CDP capture and durable-locator selection.
#[derive(Clone, Debug, PartialEq)]
pub struct CapturedElement {
    pub identity: LocatorIdentity,
    pub backend_node_id: i64,
    pub parent: Option<LocatorIdentity>,
    pub node: SemanticNode,
}

/// Integration input for one complete, already-bounded semantic capture.
#[derive(Clone, Debug, PartialEq)]
pub struct CapturedSnapshot {
    pub viewport: Viewport,
    pub scroll: Scroll,
    pub elements: Vec<CapturedElement>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionSnapshot {
    pub generation: u64,
    pub viewport: Viewport,
    pub scroll: Scroll,
    pub elements: Vec<SemanticElement>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DiffElement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<LocatorIdentity>,
    #[serde(flatten)]
    pub node: SemanticNode,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChangedElement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_parent: Option<LocatorIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_parent: Option<LocatorIdentity>,
    pub before: SemanticNode,
    pub after: SemanticNode,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SnapshotDiff {
    pub from_generation: u64,
    pub to_generation: u64,
    pub added: BTreeMap<LocatorIdentity, DiffElement>,
    pub changed: BTreeMap<LocatorIdentity, ChangedElement>,
    pub removed: BTreeMap<LocatorIdentity, DiffElement>,
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ReferenceError {
    #[serde(rename = "unknown_ref")]
    #[error("unknown element reference {reference}")]
    Unknown {
        #[serde(rename = "ref")]
        reference: ElementRef,
    },
    #[serde(rename = "stale_ref")]
    #[error("stale element reference {reference}")]
    Stale {
        #[serde(rename = "ref")]
        reference: ElementRef,
    },
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SnapshotError {
    #[error("snapshot generation space exhausted")]
    GenerationExhausted,
    #[error("element reference space exhausted")]
    ReferenceExhausted,
    #[error("duplicate durable locator identity {0:?}")]
    DuplicateIdentity(LocatorIdentity),
    #[error("element {identity:?} has missing parent {parent:?}")]
    MissingParent {
        identity: LocatorIdentity,
        parent: LocatorIdentity,
    },
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DiffError {
    #[error("snapshot generation {0} is not retained")]
    NotRetained(u64),
    #[error("snapshots {from} and {to} are not adjacent retained snapshots")]
    NotAdjacent { from: u64, to: u64 },
}

pub struct SnapshotStore {
    retention: NonZeroUsize,
    generation: u64,
    last_ref: u64,
    latest: BTreeMap<ElementRef, (LocatorIdentity, i64)>,
    snapshots: VecDeque<StoredSnapshot>,
}

#[derive(Clone)]
struct StoredSnapshot {
    response: SessionSnapshot,
    elements: BTreeMap<LocatorIdentity, DiffElement>,
}

impl Default for SnapshotStore {
    fn default() -> Self {
        // Invariant: 2 is a compile-time non-zero constant.
        #[allow(clippy::expect_used)]
        Self::new(NonZeroUsize::new(2).expect("two is non-zero"))
    }
}

impl SnapshotStore {
    pub fn new(retention: NonZeroUsize) -> Self {
        Self {
            retention,
            generation: 0,
            last_ref: 0,
            latest: BTreeMap::new(),
            snapshots: VecDeque::with_capacity(retention.get()),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn latest_snapshot(&self) -> Option<&SessionSnapshot> {
        self.snapshots.back().map(|snapshot| &snapshot.response)
    }

    /// Invalidates all issued references before a browser-mutating operation starts.
    pub fn invalidate_for_mutation(&mut self) {
        self.latest.clear();
    }

    pub fn resolve(
        &self,
        reference: ElementRef,
    ) -> Result<(&LocatorIdentity, i64), ReferenceError> {
        if let Some((identity, backend_node_id)) = self.latest.get(&reference) {
            return Ok((identity, *backend_node_id));
        }
        Err(if reference.0 <= self.last_ref {
            ReferenceError::Stale { reference }
        } else {
            ReferenceError::Unknown { reference }
        })
    }

    pub fn validate_diff_from(&self, from: u64) -> Result<(), DiffError> {
        if !self
            .snapshots
            .iter()
            .any(|snapshot| snapshot.response.generation == from)
        {
            return Err(DiffError::NotRetained(from));
        }
        if from != self.generation {
            return Err(DiffError::NotAdjacent {
                from,
                to: self.generation.saturating_add(1),
            });
        }
        Ok(())
    }

    pub fn publish(&mut self, capture: CapturedSnapshot) -> Result<SessionSnapshot, SnapshotError> {
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(SnapshotError::GenerationExhausted)?;
        let count =
            u64::try_from(capture.elements.len()).map_err(|_| SnapshotError::ReferenceExhausted)?;
        self.last_ref
            .checked_add(count)
            .ok_or(SnapshotError::ReferenceExhausted)?;

        let identities = capture
            .elements
            .iter()
            .map(|element| element.identity.clone())
            .collect::<BTreeSet<_>>();
        if identities.len() != capture.elements.len() {
            let mut seen = BTreeSet::new();
            if let Some(duplicate) = capture
                .elements
                .iter()
                .find(|element| !seen.insert(element.identity.clone()))
            {
                return Err(SnapshotError::DuplicateIdentity(duplicate.identity.clone()));
            }
        }
        for element in &capture.elements {
            if let Some(parent) = &element.parent
                && !identities.contains(parent)
            {
                return Err(SnapshotError::MissingParent {
                    identity: element.identity.clone(),
                    parent: parent.clone(),
                });
            }
        }

        let first_ref = self.last_ref.saturating_add(1);
        let references = capture
            .elements
            .iter()
            .enumerate()
            .map(|(index, element)| {
                u64::try_from(index)
                    .map_err(|_| SnapshotError::ReferenceExhausted)
                    .map(|index| (element.identity.clone(), ElementRef(first_ref + index)))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let mut latest = BTreeMap::new();
        let mut stored = BTreeMap::new();
        let elements = capture
            .elements
            .into_iter()
            .map(|element| {
                let element_ref = references[&element.identity];
                latest.insert(
                    element_ref,
                    (element.identity.clone(), element.backend_node_id),
                );
                stored.insert(
                    element.identity.clone(),
                    DiffElement {
                        parent: element.parent.clone(),
                        node: element.node.clone(),
                    },
                );
                SemanticElement {
                    element_ref,
                    identity: element.identity,
                    parent: element.parent.map(|parent| references[&parent]),
                    node: element.node,
                }
            })
            .collect();
        let response = SessionSnapshot {
            generation,
            viewport: capture.viewport,
            scroll: capture.scroll,
            elements,
            truncated: capture.truncated,
        };

        self.generation = generation;
        self.last_ref += count;
        self.latest = latest;
        self.snapshots.push_back(StoredSnapshot {
            response: response.clone(),
            elements: stored,
        });
        while self.snapshots.len() > self.retention.get() {
            self.snapshots.pop_front();
        }
        Ok(response)
    }

    pub fn diff(&self, from: u64, to: u64) -> Result<SnapshotDiff, DiffError> {
        let from_index = self
            .snapshots
            .iter()
            .position(|snapshot| snapshot.response.generation == from)
            .ok_or(DiffError::NotRetained(from))?;
        let to_index = self
            .snapshots
            .iter()
            .position(|snapshot| snapshot.response.generation == to)
            .ok_or(DiffError::NotRetained(to))?;
        if to_index != from_index + 1 {
            return Err(DiffError::NotAdjacent { from, to });
        }
        let before = &self.snapshots[from_index].elements;
        let after = &self.snapshots[to_index].elements;

        Ok(SnapshotDiff {
            from_generation: from,
            to_generation: to,
            added: after
                .iter()
                .filter(|(identity, _)| !before.contains_key(*identity))
                .map(|(identity, element)| (identity.clone(), element.clone()))
                .collect(),
            changed: after
                .iter()
                .filter_map(|(identity, current)| {
                    before
                        .get(identity)
                        .filter(|old| *old != current)
                        .map(|old| {
                            (
                                identity.clone(),
                                ChangedElement {
                                    before_parent: old.parent.clone(),
                                    after_parent: current.parent.clone(),
                                    before: old.node.clone(),
                                    after: current.node.clone(),
                                },
                            )
                        })
                })
                .collect(),
            removed: before
                .iter()
                .filter(|(identity, _)| !after.contains_key(*identity))
                .map(|(identity, element)| (identity.clone(), element.clone()))
                .collect(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(name: &str) -> SemanticNode {
        SemanticNode {
            role: "button".to_owned(),
            name: Some(name.to_owned()),
            value: None,
            description: None,
            bounds: Some(Bounds {
                x: 10.0,
                y: 20.0,
                width: 80.0,
                height: 30.0,
            }),
            visible: Some(true),
            state: SemanticState {
                enabled: Some(true),
                ..SemanticState::default()
            },
        }
    }

    fn capture(elements: &[(&str, Option<&str>, &str)]) -> CapturedSnapshot {
        CapturedSnapshot {
            viewport: Viewport {
                width: 1280,
                height: 720,
            },
            scroll: Scroll::default(),
            truncated: false,
            elements: elements
                .iter()
                .enumerate()
                .map(|(index, (identity, parent, name))| CapturedElement {
                    identity: LocatorIdentity((*identity).to_owned()),
                    backend_node_id: i64::try_from(index + 1).unwrap(),
                    parent: parent.map(|parent| LocatorIdentity(parent.to_owned())),
                    node: node(name),
                })
                .collect(),
        }
    }

    fn store(retention: usize) -> SnapshotStore {
        SnapshotStore::new(NonZeroUsize::new(retention).unwrap())
    }

    #[test]
    fn response_is_compact_and_element_refs_are_strings() {
        let mut store = store(2);
        let snapshot = store
            .publish(capture(&[("submit", None, "Submit")]))
            .unwrap();
        let value = serde_json::to_value(snapshot).unwrap();

        assert_eq!(value["elements"][0]["ref"], "e1");
        assert!(value["elements"][0].get("identity").is_none());
        assert_eq!(value["elements"][0]["role"], "button");
        assert_eq!(value["elements"][0]["visible"], true);
        assert_eq!(value["elements"][0]["states"], json!({ "enabled": true }));
        assert!(value["elements"][0].get("value").is_none());
        assert_eq!(
            serde_json::from_value::<ElementRef>(json!("e1"))
                .unwrap()
                .number(),
            1
        );
        assert!(serde_json::from_value::<ElementRef>(json!("e0")).is_err());
    }

    #[test]
    fn references_never_repeat_and_only_the_latest_snapshot_resolves() {
        let mut store = store(2);
        let first = store
            .publish(capture(&[("submit", None, "Submit")]))
            .unwrap();
        let e1 = first.elements[0].element_ref;
        assert_eq!(
            store.resolve(e1).unwrap(),
            (&LocatorIdentity("submit".into()), 1)
        );

        store.invalidate_for_mutation();
        assert_eq!(
            store.resolve(e1),
            Err(ReferenceError::Stale { reference: e1 })
        );
        assert_eq!(
            serde_json::to_value(ReferenceError::Stale { reference: e1 }).unwrap(),
            json!({ "code": "stale_ref", "ref": "e1" })
        );
        let second = store
            .publish(capture(&[("submit", None, "Submit")]))
            .unwrap();
        let e2 = second.elements[0].element_ref;
        assert_eq!(e2.to_string(), "e2");
        assert_eq!(
            store.resolve(e1),
            Err(ReferenceError::Stale { reference: e1 })
        );
        assert_eq!(
            store.resolve("e99".parse().unwrap()),
            Err(ReferenceError::Unknown {
                reference: "e99".parse().unwrap()
            })
        );
    }

    #[test]
    fn invalid_capture_does_not_replace_the_latest_registry() {
        let mut store = store(2);
        let first = store
            .publish(capture(&[("submit", None, "Submit")]))
            .unwrap();
        let reference = first.elements[0].element_ref;

        let error = store
            .publish(capture(&[("field", Some("missing"), "Field")]))
            .unwrap_err();
        assert!(matches!(error, SnapshotError::MissingParent { .. }));
        assert_eq!(store.generation(), 1);
        assert_eq!(
            store.resolve(reference).unwrap(),
            (&LocatorIdentity("submit".into()), 1)
        );
    }

    #[test]
    fn diffs_use_durable_identity_instead_of_snapshot_refs() {
        let mut store = store(3);
        store
            .publish(capture(&[
                ("form", None, "Checkout"),
                ("submit", Some("form"), "Pay"),
                ("old", None, "Old"),
            ]))
            .unwrap();
        store
            .publish(capture(&[
                ("form", None, "Checkout"),
                ("submit", Some("form"), "Pay now"),
                ("new", None, "New"),
            ]))
            .unwrap();

        let diff = store.diff(1, 2).unwrap();
        assert!(diff.added.contains_key(&LocatorIdentity("new".into())));
        let change = &diff.changed[&LocatorIdentity("submit".into())];
        assert_eq!(change.before.name.as_deref(), Some("Pay"));
        assert_eq!(change.after.name.as_deref(), Some("Pay now"));
        assert!(diff.removed.contains_key(&LocatorIdentity("old".into())));
        assert_eq!(
            store.latest_snapshot().unwrap().elements[0]
                .element_ref
                .to_string(),
            "e4"
        );
    }

    #[test]
    fn retention_is_bounded_and_diffs_require_adjacent_snapshots() {
        let mut store = store(2);
        for name in ["One", "Two", "Three"] {
            store.publish(capture(&[("button", None, name)])).unwrap();
        }

        assert_eq!(store.diff(1, 2), Err(DiffError::NotRetained(1)));
        assert_eq!(
            store.diff(3, 2),
            Err(DiffError::NotAdjacent { from: 3, to: 2 })
        );
        assert!(store.diff(2, 3).is_ok());
    }

    #[test]
    fn publish_rejects_duplicate_element_identities() {
        let mut store = store(2);
        let mut snapshot = capture(&[("button", None, "One"), ("button", None, "Two")]);
        snapshot.elements[1].identity = snapshot.elements[0].identity.clone();
        assert!(matches!(
            store.publish(snapshot).unwrap_err(),
            SnapshotError::DuplicateIdentity(_)
        ));
    }

    #[test]
    fn invalid_diff_baselines_are_rejected_before_publishing_an_unseen_snapshot() {
        let mut store = store(2);
        store.publish(capture(&[("button", None, "One")])).unwrap();
        store.publish(capture(&[("button", None, "Two")])).unwrap();
        let current_ref = store.latest_snapshot().unwrap().elements[0].element_ref;

        assert_eq!(
            store.validate_diff_from(1),
            Err(DiffError::NotAdjacent { from: 1, to: 3 })
        );
        assert_eq!(store.generation(), 2);
        assert!(store.resolve(current_ref).is_ok());
        assert!(store.validate_diff_from(2).is_ok());
    }
}
