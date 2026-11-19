//! Port `store-visit-form-revise-mode.ts` (enum Postgres `StoreVisitFormReviseMode`).

pub const STORE_VISIT_FORM_REVISE_MODES: [&str; 2] = ["allPrivileged", "selected"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviseMode {
    AllPrivileged,
    Selected,
}

impl ReviseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllPrivileged => "allPrivileged",
            Self::Selected => "selected",
        }
    }

    /// `isStoreVisitFormReviseMode(value) ? value : undefined`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allPrivileged" => Some(Self::AllPrivileged),
            "selected" => Some(Self::Selected),
            _ => None,
        }
    }
}

/// `isStoreVisitFormReviseMode`.
pub fn is_store_visit_form_revise_mode(v: Option<&serde_json::Value>) -> bool {
    v.and_then(serde_json::Value::as_str)
        .and_then(ReviseMode::parse)
        .is_some()
}
