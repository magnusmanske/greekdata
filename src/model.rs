//! Domain types shared by every source, the database layer, and the API.

use crate::{Error, Result};
use jiff::civil::{Date, DateTime};
use serde::{Deserialize, Serialize};

/// Defines a string-backed enum together with its `as_str`/`Display`/`FromStr` plumbing,
/// so the same three impls are not hand-written for every vocabulary in the crate.
macro_rules! str_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = Error;
            fn from_str(text: &str) -> Result<Self> {
                match text {
                    $($text => Ok(Self::$variant),)+
                    other => Err(Error::parse(stringify!($name), format!("unknown value `{other}`"))),
                }
            }
        }
    };
}

str_enum! {
    /// What a stored entity actually is.
    EntityKind {
        Pharmacy => "pharmacy",
        Hospital => "hospital",
        HealthCentre => "health_centre",
        Cinema => "cinema",
        Film => "film",
    }
}

str_enum! {
    /// A themed collection of sources, as listed in `AGENTS.md`.
    DataGroup {
        Pharmacies => "pharmacies",
        Hospitals => "hospitals",
        Cinemas => "cinemas",
    }
}

str_enum! {
    /// How seriously to take an ingest problem.
    Severity {
        Info => "info",
        Warning => "warning",
        Error => "error",
    }
}

/// A geographic point, checked on construction so an out-of-range coordinate can never
/// reach the database.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Location {
    lat: f64,
    lon: f64,
}

impl Location {
    pub fn new(lat: f64, lon: f64) -> Result<Self> {
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            return Err(Error::parse(
                "location",
                format!("coordinates out of range: {lat}, {lon}"),
            ));
        }
        Ok(Self { lat, lon })
    }

    pub fn lat(self) -> f64 {
        self.lat
    }

    pub fn lon(self) -> f64 {
        self.lon
    }
}

/// An identifier assigned by somebody else: `wikidata`/`Q42`, `imdb`/`tt0111161`,
/// or a source's own key such as `fsa-attica-pharmacies`/`288990`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalId {
    pub scheme: String,
    pub value: String,
}

impl ExternalId {
    pub fn new(scheme: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            scheme: scheme.into(),
            value: value.into(),
        }
    }
}

/// How a source identifies an entity across its own documents.
///
/// This decides what may safely be used to match a draft against a stored entity. Two
/// pharmacies can share a pharmacist's name, so a source that assigns its own key must
/// not fall back to name matching; a hospital rota gives nothing but a name, so there
/// the name and its recorded aliases are all we have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Identity {
    /// The source assigns a stable key. Names are descriptive only and may collide.
    SourceKey,
    /// The name, folded, is the identifier.
    #[default]
    Name,
}

/// An entity as a source describes it, before it has been matched to a stored entity.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityDraft {
    pub kind: EntityKind,
    /// What may be used to recognize this entity again.
    pub identity: Identity,
    /// The source's own stable key for this entity. Records sharing a `local_id` within
    /// one document describe the same thing and are merged during ingest.
    pub local_id: String,
    pub name: String,
    pub address: Option<String>,
    pub municipality: Option<String>,
    pub location: Option<Location>,
    pub url: Option<String>,
    pub phone: Option<String>,
    pub external_ids: Vec<ExternalId>,
    /// Alternative spellings seen in the wild, used to match future messy documents.
    pub aliases: Vec<String>,
}

impl EntityDraft {
    pub fn new(kind: EntityKind, local_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind,
            identity: Identity::default(),
            local_id: local_id.into(),
            name: name.into(),
            address: None,
            municipality: None,
            location: None,
            url: None,
            phone: None,
            external_ids: Vec::new(),
            aliases: Vec::new(),
        }
    }

    pub fn identified_by(mut self, identity: Identity) -> Self {
        self.identity = identity;
        self
    }
}

/// The group-specific detail of a property. Adding a data group means adding a variant
/// here; the storage, provenance and API layers stay untouched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PropertyPayload {
    /// A pharmacy holding an out-of-hours duty shift.
    PharmacyOnCall {
        pharmacist: Option<String>,
        /// The opening hours exactly as published, kept because the published wording
        /// is often looser than the times we manage to parse out of it.
        hours_text: Option<String>,
    },
    /// A hospital taking admissions for one clinical speciality during one shift.
    HospitalOnCall {
        clinic: String,
        shift: String,
        notes: Option<String>,
    },
    /// A health centre on duty (published alongside hospitals, but a distinct kind).
    HealthCentreOnCall,
}

impl PropertyPayload {
    /// The property vocabulary term stored in `property.kind` and exposed by the API.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::PharmacyOnCall { .. } => "on_call",
            Self::HospitalOnCall { .. } => "on_call",
            Self::HealthCentreOnCall => "on_call",
        }
    }
}

/// Something true of an entity on a given day, optionally narrowed to a time window.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyDraft {
    pub on_date: Date,
    pub starts_at: Option<DateTime>,
    pub ends_at: Option<DateTime>,
    pub payload: PropertyPayload,
}

/// One entity plus everything a document says about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub entity: EntityDraft,
    pub properties: Vec<PropertyDraft>,
}

/// A data problem worth surfacing. Warnings are recorded, not raised: one unreadable
/// table row must not discard the rest of a document.
#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    pub severity: Severity,
    pub code: &'static str,
    pub detail: String,
}

impl Warning {
    pub fn new(severity: Severity, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            severity,
            code,
            detail: detail.into(),
        }
    }

    pub fn warn(code: &'static str, detail: impl Into<String>) -> Self {
        Self::new(Severity::Warning, code, detail)
    }
}

/// The complete result of parsing one source document.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Extraction {
    pub records: Vec<Record>,
    pub warnings: Vec<Warning>,
}

/// Which reissue of a document this is. Greek ministry files are frequently republished
/// as `ΟΡΘΗ ΕΠΑΝΑΚΟΙΝΟΠΟΙΗΣΗ` ("corrected reissue"); the highest revision for a date wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct Revision(pub u32);

impl Revision {
    pub const ORIGINAL: Self = Self(0);
}

impl std::fmt::Display for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn entity_kind_round_trips_through_text() {
        for kind in EntityKind::ALL {
            assert_eq!(EntityKind::from_str(kind.as_str()).ok(), Some(*kind));
        }
    }

    #[test]
    fn unknown_entity_kind_is_an_error_not_a_panic() {
        assert!(EntityKind::from_str("submarine").is_err());
    }

    #[test]
    fn location_rejects_impossible_coordinates() {
        assert!(Location::new(37.98, 23.72).is_ok());
        assert!(Location::new(91.0, 23.72).is_err());
        assert!(Location::new(37.98, -181.0).is_err());
    }
}
