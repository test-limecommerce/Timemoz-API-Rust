//! Port `src/admin-jobs/company-data/company-scope.ts`: peta "apa saja yang termasuk milik
//! sebuah company" — graf model dari DMMF Prisma, tambalan relasi logis, aturan cakupan.
//!
//! DMMF Prisma tidak tersedia di Rust, jadi `Prisma.dmmf.datamodel` Mudah-API ditanam sebagai
//! `dmmf.json` (`tools/dump-dmmf.cjs`). Sidik jari schema dihitung dari berkas itu dan harus
//! sama dengan Node; backup/restore juga mencocokkannya dengan katalog Postgres
//! ([`super::pg::Catalog::verify`]) supaya berkas yang basi tidak diam-diam membuang kolom.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use indexmap::{IndexMap, IndexSet};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::errors::{CdResult, Code, fail};

/// Satu baris data apa adanya (hasil query atau hasil `JSON.parse` backup).
pub type Row = Map<String, Value>;

const SCOPE_FILE: &str = "src/admin-jobs/company-data/company-scope.ts";

// ---------------------------------------------------------------------------
// DMMF tertanam
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DmmfField {
    pub name: String,
    pub kind: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub is_list: bool,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub is_unique: bool,
    #[serde(default)]
    pub is_id: bool,
    #[serde(default)]
    pub has_default_value: bool,
    #[serde(default)]
    pub relation_from_fields: Vec<String>,
    #[serde(default)]
    pub relation_to_fields: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DmmfModel {
    pub name: String,
    #[serde(default)]
    pub db_name: Option<String>,
    #[serde(default)]
    pub primary_key: Option<Vec<String>>,
    pub fields: Vec<DmmfField>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DmmfEnum {
    pub name: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Datamodel {
    pub models: Vec<DmmfModel>,
    pub enums: Vec<DmmfEnum>,
}

static DMMF_JSON: &str = include_str!("dmmf.json");

/// `Prisma.dmmf.datamodel` Mudah-API (berkas tertanam). `None` hanya bila berkasnya rusak.
pub fn datamodel() -> Option<&'static Datamodel> {
    static DM: LazyLock<Option<Datamodel>> =
        LazyLock::new(|| match serde_json::from_str(DMMF_JSON) {
            Ok(dm) => Some(dm),
            Err(e) => {
                tracing::error!(error = %e, "dmmf.json tertanam rusak");
                None
            }
        });
    DM.as_ref()
}

// ---------------------------------------------------------------------------
// 1. Tambalan relasi logis (kolom FK tanpa @relation)
// ---------------------------------------------------------------------------

/// `LOGICAL_FKS`: model -> [(kolom, modelTujuan)].
pub const LOGICAL_FKS: &[(&str, &[(&str, &str)])] = &[
    ("jwtBlackList", &[("userId", "users")]),
    ("accessLevel", &[("createdById", "users")]),
    ("userForgotPassword", &[("userId", "users")]),
    ("userToken", &[("userConnectedId", "userConnected")]),
    ("syncDbLocal", &[("userId", "users")]),
    ("operatorForgotPassword", &[("operatorId", "operators")]),
    ("dailyProjectSummary", &[("companyId", "company")]),
    ("dailyUserProjectSummary", &[("companyId", "company")]),
    ("subscriptions", &[("pendingPricingTierId", "pricingTiers")]),
    (
        "mobileShift",
        &[
            ("clockInGeofenceId", "geofence"),
            ("clockOutGeofenceId", "geofence"),
        ],
    ),
    ("storeGroupUser", &[("userId", "users")]),
    ("storeVisitPlan", &[("userId", "users")]),
    (
        "storeVisitFormTemplate",
        &[("companyId", "company"), ("createdById", "users")],
    ),
    (
        "storeVisitFormTemplateVersion",
        &[("publishedById", "users")],
    ),
    ("storeVisitFormTemplateUser", &[("userId", "users")]),
    (
        "storeVisitFormTemplateAsset",
        &[("companyId", "company"), ("createdById", "users")],
    ),
    (
        "storeVisitFormSubmission",
        &[
            ("companyId", "company"),
            ("visitId", "storeVisit"),
            ("userId", "users"),
            ("storeId", "store"),
            ("templateId", "storeVisitFormTemplate"),
            ("templateVersionId", "storeVisitFormTemplateVersion"),
            ("lastEditedById", "users"),
        ],
    ),
    (
        "storeVisitFormSubmissionRevision",
        &[("editedById", "users")],
    ),
    (
        "storeVisitFormReviser",
        &[
            ("companyId", "company"),
            ("userId", "users"),
            ("createdById", "users"),
        ],
    ),
    (
        "auditLog",
        &[
            ("companyId", "company"),
            ("actorId", "users"),
            ("restoredById", "users"),
        ],
    ),
    (
        "fieldRevealLog",
        &[("companyId", "company"), ("actorId", "users")],
    ),
    (
        "fieldRevealQuota",
        &[("companyId", "company"), ("actorId", "users")],
    ),
];

/// Predikat `LIST_FKS[..].when`.
pub type RowPredicate = fn(&Row) -> bool;

fn geofence_user_ids(row: &Row) -> bool {
    row.get("applicableTo").and_then(Value::as_str) == Some("user_ids")
}

/// Satu `LIST_FKS[model][kolom]`: (kolom, target, when).
pub type ListFkSpec = (&'static str, &'static str, RowPredicate);

/// `LIST_FKS`: model -> [(kolom, target, when)].
pub const LIST_FKS: &[(&str, &[ListFkSpec])] =
    &[("geofence", &[("applicableIds", "users", geofence_user_ids)])];

/// `PolymorphicSpec`.
#[derive(Debug)]
pub struct PolymorphicSpec {
    pub discriminator: &'static str,
    pub id_column: &'static str,
    pub uuid_column: &'static str,
    pub targets: &'static [(&'static str, &'static str)],
}

impl PolymorphicSpec {
    /// `targets[value]`.
    pub fn target(&self, value: &str) -> Option<&'static str> {
        self.targets
            .iter()
            .find(|(k, _)| *k == value)
            .map(|(_, t)| *t)
    }
}

/// `POLYMORPHIC_FKS`.
pub const POLYMORPHIC_FKS: &[(&str, PolymorphicSpec)] = &[
    (
        "auditLog",
        PolymorphicSpec {
            discriminator: "entityType",
            id_column: "entityId",
            uuid_column: "entityUuid",
            targets: &[
                ("company", "company"),
                ("employee", "users"),
                ("store", "store"),
                ("storeGroup", "storeGroup"),
                ("storeVisitForm", "storeVisitFormTemplate"),
                ("storeVisitPlan", "storeVisitPlan"),
                ("geofence", "geofence"),
                ("negativeZone", "negativeZone"),
                ("role", "accessLevel"),
                ("customer", "customer"),
                ("customerType", "customerType"),
                ("customerGroup", "customerGroup"),
                ("customerChannel", "customerChannel"),
                ("customerSubChannel", "customerSubChannel"),
                ("product", "product"),
                ("productCategory", "productCategory"),
                ("productSubCategory", "productSubCategory"),
                ("productBrand", "productBrand"),
                ("productPrincipal", "productPrincipal"),
                ("uom", "uom"),
                ("taxRate", "taxRate"),
                ("priceList", "priceList"),
                ("priceListItem", "priceListItem"),
                ("productCustomField", "productCustomField"),
                ("order", "order"),
                ("orderType", "orderType"),
                ("orderReason", "orderReason"),
                ("orderPaymentTerm", "orderPaymentTerm"),
                ("orderPaymentMethod", "orderPaymentMethod"),
                ("orderCustomFieldDefinition", "orderCustomFieldDefinition"),
                ("visitType", "visitType"),
                ("visitFrequency", "visitFrequency"),
                ("division", "division"),
            ],
        },
    ),
    (
        "fieldRevealLog",
        PolymorphicSpec {
            discriminator: "tableKey",
            id_column: "recordId",
            uuid_column: "recordUuid",
            targets: &[
                ("stores", "store"),
                ("customers", "customer"),
                ("products", "product"),
            ],
        },
    ),
];

/// `LOGICAL_UUID_REFS`.
pub const LOGICAL_UUID_REFS: &[(&str, &[(&str, &str)])] = &[
    ("storeVisitFormSubmission", &[("visitUuid", "storeVisit")]),
    ("auditLog", &[("restoredByLogId", "auditLog")]),
];

// ---------------------------------------------------------------------------
// 2. Titik jangkar
// ---------------------------------------------------------------------------

/// `AnchorConfig`: `via = None` berarti `self: true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub name: &'static str,
    pub via: Option<(&'static str, &'static str)>,
}

/// `ANCHORS` (urutan kunci = urutan objek Node).
pub const ANCHORS: &[Anchor] = &[
    Anchor {
        name: "company",
        via: None,
    },
    Anchor {
        name: "users",
        via: Some(("userCompany", "userId")),
    },
    Anchor {
        name: "project",
        via: Some(("projectCompany", "projectId")),
    },
    Anchor {
        name: "client",
        via: Some(("companyClient", "clientId")),
    },
];

pub fn anchor(name: &str) -> Option<&'static Anchor> {
    ANCHORS.iter().find(|a| a.name == name)
}

// ---------------------------------------------------------------------------
// 3. Model global / separuh global / ephemeral / heavy
// ---------------------------------------------------------------------------

/// `EXTERNAL_MODELS`: model -> natural key (`None` = tidak pernah dicocokkan ulang).
pub const EXTERNAL_MODELS: &[(&str, Option<&[&str]>)] = &[
    ("projectType", Some(&["code"])),
    ("pricingTiers", Some(&["name", "billingInterval"])),
    ("operators", Some(&["username"])),
    ("prohibitedApps", Some(&["appName", "platform"])),
    ("appRelease", Some(&["version", "platform", "channel"])),
    ("latestVersion", None),
    ("latestVersionMobile", None),
    ("setting", Some(&["code"])),
    ("mcpOAuthClient", None),
    ("paypalWebhookEvents", None),
    ("mixpanelEvent", None),
    ("operatorForgotPassword", None),
    ("mcpAdminAccessToken", None),
    ("mcpAdminOAuthCode", None),
    ("mcpAdminOAuthAccessToken", None),
    ("mcpAdminOAuthRefreshToken", None),
    ("nationalHoliday", None),
    ("adminJob", None),
    ("operatorAuditLog", None),
];

pub fn external_keys(name: &str) -> Option<Option<&'static [&'static str]>> {
    EXTERNAL_MODELS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, k)| *k)
}

/// `HybridModelConfig`. `match` selalu `{ <scopeColumn>: null }` di schema ini.
#[derive(Debug)]
pub struct HybridConfig {
    pub name: &'static str,
    pub natural_key: &'static [&'static str],
    /// `match`: kolom yang wajib NULL (pasangan `{ kolom: null }`).
    pub match_null: &'static [&'static str],
    pub scope_column: &'static str,
}

pub const HYBRID_MODELS: &[HybridConfig] = &[HybridConfig {
    name: "accessLevel",
    natural_key: &["code"],
    match_null: &["companyId"],
    scope_column: "companyId",
}];

pub fn hybrid(name: &str) -> Option<&'static HybridConfig> {
    HYBRID_MODELS.iter().find(|h| h.name == name)
}

/// `EPHEMERAL_MODELS`.
pub const EPHEMERAL_MODELS: &[&str] = &[
    "jwtBlackList",
    "userForgotPassword",
    "userToken",
    "userConnected",
    "syncDbLocal",
    "userLocalDb",
    "analyticsEvent",
    "mcpAccessToken",
    "mcpAccessTokenLog",
    "mcpOAuthCode",
    "mcpOAuthAccessToken",
    "mcpOAuthAccessTokenLog",
    "mcpOAuthRefreshToken",
    "mcpAdminAccessToken",
    "mcpAdminOAuthCode",
    "mcpAdminOAuthAccessToken",
    "mcpAdminOAuthRefreshToken",
    "timesheetApprovalToken",
    "manualTimeApprovalToken",
    "fieldRevealQuota",
];

/// `HEAVY_MODELS`.
pub const HEAVY_MODELS: &[&str] = &[
    "tracking",
    "trackingTimeline",
    "screenshot",
    "app",
    "url",
    "dailyProjectSummary",
    "dailyUserProjectSummary",
    "dailyCompanySummary",
    "userLog",
    "geofenceAuditLog",
    "userLocationPing",
    "storeVisitAuditLog",
    "auditLog",
    "fieldRevealLog",
];

// ---------------------------------------------------------------------------
// 4. Tabrakan unique saat restore
// ---------------------------------------------------------------------------

/// `UniqueConflictPolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Reuse,
    Null,
    Regenerate,
    Suffix,
    Fail,
    Skip,
}

/// `UNIQUE_CONFLICT_POLICY`.
pub const UNIQUE_CONFLICT_POLICY: &[(&str, &[(&str, Policy)])] = &[
    (
        "users",
        &[("username", Policy::Reuse), ("googleId", Policy::Null)],
    ),
    (
        "company",
        &[
            ("inviteCode", Policy::Regenerate),
            ("discordGuildId", Policy::Null),
        ],
    ),
    ("store", &[("qrToken", Policy::Regenerate)]),
    ("timesheetInvoice", &[("invoiceNumber", Policy::Suffix)]),
    ("userKycFace", &[("userId", Policy::Skip)]),
    ("operators", &[("username", Policy::Reuse)]),
];

pub fn policy(model: &str, column: &str) -> Policy {
    UNIQUE_CONFLICT_POLICY
        .iter()
        .find(|(m, _)| *m == model)
        .and_then(|(_, cols)| cols.iter().find(|(c, _)| *c == column))
        .map_or(Policy::Fail, |(_, p)| *p)
}

const INVITE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// `randomAlnum(length)`.
pub fn random_alnum(length: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; length];
    rand::rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| {
            let idx = usize::from(*b) % INVITE_ALPHABET.len();
            char::from(INVITE_ALPHABET.get(idx).copied().unwrap_or(b'A'))
        })
        .collect()
}

/// `REGENERATORS[model.col]`, bawaan `randomUUID()`.
pub fn regenerate(model: &str, column: &str) -> String {
    match (model, column) {
        ("company", "inviteCode") => random_alnum(12),
        ("store", "qrToken") => {
            use rand::RngCore;
            let mut b = [0u8; 16];
            rand::rng().fill_bytes(&mut b);
            hex::encode(b)
        }
        _ => uuid::Uuid::new_v4().to_string(),
    }
}

/// `REGENERATE_ON_CLONE`.
pub const REGENERATE_ON_CLONE: &[(&str, &[&str])] = &[
    ("storeVisit", &["clientRequestId"]),
    ("storeVisitFormSubmission", &["clientRequestId"]),
];

pub fn regenerate_on_clone(model: &str) -> &'static [&'static str] {
    REGENERATE_ON_CLONE
        .iter()
        .find(|(m, _)| *m == model)
        .map_or(&[], |(_, c)| *c)
}

/// `onConflict` sebuah partial unique index.
#[derive(Debug)]
pub enum PartialAction {
    /// Keluarkan baris dari predikat: `row[column] = value`.
    Set {
        column: &'static str,
        value: bool,
    },
    Fail {
        message: &'static str,
    },
}

/// `PartialUniqueIndexSpec`. `where` + `applies` di Node selalu satu kolom = satu nilai.
#[derive(Debug)]
pub struct PartialUniqueSpec {
    pub name: &'static str,
    pub model: &'static str,
    pub keys: &'static [&'static str],
    /// Predikat index: kolom dan nilai teksnya (dibandingkan sebagai `kolom::text`).
    pub where_column: &'static str,
    pub where_value: Value0,
    pub on_conflict: PartialAction,
}

/// Nilai predikat sederhana (`true` / string enum).
#[derive(Debug, Clone, Copy)]
pub enum Value0 {
    Bool(bool),
    Text(&'static str),
}

impl PartialUniqueSpec {
    /// `applies(row)`: nilai baris sama persis (`===`) dengan predikat.
    pub fn applies(&self, row: &Row) -> bool {
        match (self.where_value, row.get(self.where_column)) {
            (Value0::Bool(b), Some(Value::Bool(v))) => *v == b,
            (Value0::Text(t), Some(Value::String(v))) => v == t,
            _ => false,
        }
    }
}

/// `PARTIAL_UNIQUE_INDEXES`.
pub const PARTIAL_UNIQUE_INDEXES: &[PartialUniqueSpec] = &[
    PartialUniqueSpec {
        name: "userCompany_one_default_per_user",
        model: "userCompany",
        keys: &["userId"],
        where_column: "isDefault",
        where_value: Value0::Bool(true),
        on_conflict: PartialAction::Set {
            column: "isDefault",
            value: false,
        },
    },
    PartialUniqueSpec {
        name: "storeVisit_one_active_per_user",
        model: "storeVisit",
        keys: &["userId"],
        where_column: "state",
        where_value: Value0::Text("active"),
        on_conflict: PartialAction::Fail {
            message: "User yang dipakai ulang sudah punya kunjungan berstatus active, dan hanya boleh ada satu.\n  Pilihan: --users insert --username-suffix \".copy\" (user salinan sendiri),\n  --exclude storeVisit, atau tutup dulu kunjungan yang masih berjalan di tujuan.",
        },
    },
];

/// `KNOWN_PARTIAL_UNIQUE_INDEXES`.
pub const KNOWN_PARTIAL_UNIQUE_INDEXES: &[&str] = &[
    "accessLevel_company_code_key",
    "accessLevel_company_override_key",
    "accessLevel_global_code_key",
    "adminJob_one_active_per_company",
    "customer_code_unique_per_company",
    "divisionMember_division_user_unique_active",
    "division_company_name_unique_active",
    "formTemplateUser_assignment_unique_active",
    "invoices_manual_reference_key",
    "manualTimeApprovalException_unique_active",
    "orderDelivery_one_initial_per_order",
    "orderPaymentMethod_one_default_per_company",
    "orderPaymentTerm_one_default_per_company",
    "orderType_one_default_per_company",
    "overtimeRequest_one_active_per_day",
    "priceListItem_tier_unique_active",
    "priceList_code_unique_per_company",
    "priceList_one_default_per_company",
    "productUom_one_base_per_product",
    "product_sku_unique_per_company",
    "storeGroupUser_member_unique_active",
    "storeVisitFormReviser_unique_active",
    "storeVisitPlan_slot_unique_active",
    "storeVisit_one_active_per_user",
    "store_code_unique_per_company",
    "tableFieldVisibility_company_default_key",
    "tableFieldVisibility_role_key",
    "userAccessLevel_one_active_per_company",
    "userCompany_one_default_per_user",
    "visitType_one_default_per_company",
];

/// `USER_SCOPED_UNIQUE_RISK`.
pub const USER_SCOPED_UNIQUE_RISK: &[(&str, &[&str])] = &[
    ("tracking", &["userId", "takenDate"]),
    ("userLocalDb", &["userId", "date"]),
    ("userConnected", &["userId", "macAddress"]),
    ("analyticsEvent", &["userId", "eventName"]),
];

pub fn user_scoped_risk(model: &str) -> bool {
    USER_SCOPED_UNIQUE_RISK.iter().any(|(m, _)| *m == model)
}

/// `SECRET_COLUMNS`.
pub const SECRET_COLUMNS: &[(&str, &[&str])] = &[
    (
        "users",
        &[
            "password",
            "googleCalendarAccessToken",
            "googleCalendarRefreshToken",
        ],
    ),
    (
        "company",
        &[
            "clickupApiToken",
            "jiraAccessToken",
            "jiraRefreshToken",
            "jiraWebhookSecret",
            "googleCalendarAccessToken",
            "googleCalendarRefreshToken",
        ],
    ),
    ("operators", &["password"]),
];

pub fn secret_columns(model: &str) -> &'static [&'static str] {
    SECRET_COLUMNS
        .iter()
        .find(|(m, _)| *m == model)
        .map_or(&[], |(_, c)| *c)
}

// ---------------------------------------------------------------------------
// Pembangun graf
// ---------------------------------------------------------------------------

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `FkInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkInfo {
    pub column: String,
    pub target: String,
    pub target_column: String,
    pub nullable: bool,
    pub declared: bool,
}

/// `ListFkInfo`.
#[derive(Debug, Clone)]
pub struct ListFkInfo {
    pub column: String,
    pub target: String,
    pub when: RowPredicate,
}

/// `FieldMeta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldMeta {
    pub ty: String,
    pub is_list: bool,
    pub is_required: bool,
    pub kind: String,
}

/// `ModelInfo`.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub name: String,
    pub table: String,
    pub pk: Option<String>,
    pub pk_type: Option<String>,
    pub pk_has_default: bool,
    pub composite_pk: Option<Vec<String>>,
    pub uuid_col: bool,
    pub fks: Vec<FkInfo>,
    pub list_fks: Vec<ListFkInfo>,
    pub polymorphic: Option<&'static PolymorphicSpec>,
    pub uuid_refs: &'static [(&'static str, &'static str)],
    pub scalar_fields: Vec<String>,
    /// Urutan = urutan field model (DMMF).
    pub field_meta: IndexMap<String, FieldMeta>,
    pub unique_columns: Vec<String>,
}

/// `ModelGraph` (urutan iterasi = urutan model DMMF, seperti `Map` JS).
pub type ModelGraph = IndexMap<String, ModelInfo>;

/// `resolveCompositeFk`.
fn resolve_composite_fk(
    m: &DmmfModel,
    f: &DmmfField,
    all: &[DmmfModel],
) -> Option<(String, String)> {
    let target = all.iter().find(|x| x.name == f.ty)?;
    let target_pk = target.fields.iter().find(|x| x.is_id)?.name.clone();
    let from = &f.relation_from_fields;
    let to = &f.relation_to_fields;
    if to.len() != from.len() {
        return None;
    }
    let idx = to.iter().position(|t| *t == target_pk)?;
    for (i, companion) in from.iter().enumerate() {
        if i == idx {
            continue;
        }
        let covered = m.fields.iter().any(|o| {
            !std::ptr::eq(o, f)
                && o.kind == "object"
                && o.relation_from_fields.len() == 1
                && o.relation_from_fields.first() == Some(companion)
        });
        if !covered {
            return None;
        }
    }
    Some((from.get(idx)?.clone(), to.get(idx)?.clone()))
}

/// `buildModelGraph(dm)`.
pub fn build_model_graph(dm: &Datamodel) -> CdResult<ModelGraph> {
    let enum_values: HashMap<&str, &Vec<String>> = dm
        .enums
        .iter()
        .map(|e| (e.name.as_str(), &e.values))
        .collect();
    let mut models = ModelGraph::new();

    for m in &dm.models {
        let table = m.db_name.clone().unwrap_or_else(|| m.name.clone());
        if !is_identifier(&table) {
            return fail(
                Code::ScopeConfig,
                format!("Nama tabel tidak aman untuk dipakai sebagai identifier SQL: {table}"),
            );
        }
        let pk_field = m.fields.iter().find(|f| f.is_id);
        let scalars: Vec<&DmmfField> = m.fields.iter().filter(|f| f.kind != "object").collect();
        let field_by_name: HashMap<&str, &DmmfField> =
            scalars.iter().map(|f| (f.name.as_str(), *f)).collect();

        let mut fks: Vec<FkInfo> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for f in &m.fields {
            if f.kind != "object" || f.relation_from_fields.is_empty() {
                continue;
            }
            let (column, target_column) = if f.relation_from_fields.len() > 1 {
                match resolve_composite_fk(m, f, &dm.models) {
                    Some(c) => c,
                    None => {
                        return fail(
                            Code::ScopeConfig,
                            format!("FK gabungan belum didukung: {}.{}", m.name, f.name),
                        );
                    }
                }
            } else {
                (
                    f.relation_from_fields.first().cloned().unwrap_or_default(),
                    f.relation_to_fields.first().cloned().unwrap_or_default(),
                )
            };
            let nullable = field_by_name
                .get(column.as_str())
                .is_none_or(|s| !s.is_required);
            seen.insert(column.clone());
            fks.push(FkInfo {
                column,
                target: f.ty.clone(),
                target_column,
                nullable,
                declared: true,
            });
        }

        if let Some((_, logical)) = LOGICAL_FKS.iter().find(|(n, _)| *n == m.name) {
            for (column, target) in *logical {
                if seen.contains(*column) {
                    continue;
                }
                let Some(scalar) = field_by_name.get(column) else {
                    return fail(
                        Code::ScopeConfig,
                        format!(
                            "LOGICAL_FKS menyebut {}.{column} yang tidak ada di schema. Perbarui {SCOPE_FILE}.",
                            m.name
                        ),
                    );
                };
                fks.push(FkInfo {
                    column: (*column).to_owned(),
                    target: (*target).to_owned(),
                    target_column: "id".to_owned(),
                    nullable: !scalar.is_required,
                    declared: false,
                });
                seen.insert((*column).to_owned());
            }
        }

        let mut list_fks = Vec::new();
        if let Some((_, cfg)) = LIST_FKS.iter().find(|(n, _)| *n == m.name) {
            for (column, target, when) in *cfg {
                if !field_by_name.contains_key(column) {
                    return fail(
                        Code::ScopeConfig,
                        format!(
                            "LIST_FKS menyebut {}.{column} yang tidak ada di schema.",
                            m.name
                        ),
                    );
                }
                list_fks.push(ListFkInfo {
                    column: (*column).to_owned(),
                    target: (*target).to_owned(),
                    when: *when,
                });
            }
        }

        let polymorphic = POLYMORPHIC_FKS
            .iter()
            .find(|(n, _)| *n == m.name)
            .map(|(_, p)| p);
        if let Some(poly) = polymorphic {
            if !field_by_name.contains_key(poly.id_column) {
                return fail(
                    Code::ScopeConfig,
                    format!(
                        "POLYMORPHIC_FKS menyebut kolom {}.{} yang tidak ada.",
                        m.name, poly.id_column
                    ),
                );
            }
            let Some(discriminator) = field_by_name.get(poly.discriminator) else {
                return fail(
                    Code::ScopeConfig,
                    format!(
                        "POLYMORPHIC_FKS menyebut kolom {}.{} yang tidak ada.",
                        m.name, poly.discriminator
                    ),
                );
            };
            let values = enum_values.get(discriminator.ty.as_str());
            let unmapped: Vec<&str> = values
                .map(|v| {
                    v.iter()
                        .map(String::as_str)
                        .filter(|v| poly.target(v).is_none())
                        .collect()
                })
                .unwrap_or_default();
            if !unmapped.is_empty() {
                return fail(
                    Code::ScopeConfig,
                    format!(
                        "{} nilai {} belum punya model tujuan di POLYMORPHIC_FKS.{}: {}. Tanpa itu, baris {} untuk entitas tersebut kehilangan rujukannya saat restore. Lengkapi di {SCOPE_FILE}.",
                        unmapped.len(),
                        discriminator.ty,
                        m.name,
                        unmapped.join(", "),
                        m.name
                    ),
                );
            }
            for (value, target) in poly.targets {
                if let Some(values) = values
                    && !values.iter().any(|v| v == value)
                {
                    return fail(
                        Code::ScopeConfig,
                        format!(
                            "POLYMORPHIC_FKS.{} memetakan '{value}' yang bukan nilai {}.",
                            m.name, discriminator.ty
                        ),
                    );
                }
                if !dm.models.iter().any(|x| x.name == *target) {
                    return fail(
                        Code::ScopeConfig,
                        format!(
                            "POLYMORPHIC_FKS.{}.{value} menunjuk model {target} yang tidak ada.",
                            m.name
                        ),
                    );
                }
            }
        }

        let uuid_col = m.fields.iter().any(|f| f.name == "uuid" && f.is_unique);
        let unique_columns = m
            .fields
            .iter()
            .filter(|f| f.kind != "object" && f.is_unique && !f.is_id && f.name != "uuid")
            .map(|f| f.name.clone())
            .collect();
        let uuid_refs = LOGICAL_UUID_REFS
            .iter()
            .find(|(n, _)| *n == m.name)
            .map_or(&[][..], |(_, r)| *r);

        models.insert(
            m.name.clone(),
            ModelInfo {
                name: m.name.clone(),
                table,
                pk: pk_field.map(|f| f.name.clone()),
                pk_type: pk_field.map(|f| f.ty.clone()),
                pk_has_default: pk_field.is_some_and(|f| f.has_default_value),
                composite_pk: if pk_field.is_some() {
                    None
                } else {
                    m.primary_key.clone()
                },
                uuid_col,
                fks,
                list_fks,
                polymorphic,
                uuid_refs,
                scalar_fields: scalars.iter().map(|f| f.name.clone()).collect(),
                field_meta: scalars
                    .iter()
                    .map(|f| {
                        (
                            f.name.clone(),
                            FieldMeta {
                                ty: f.ty.clone(),
                                is_list: f.is_list,
                                is_required: f.is_required,
                                kind: f.kind.clone(),
                            },
                        )
                    })
                    .collect(),
                unique_columns,
            },
        );
    }
    Ok(models)
}

/// Graf dari DMMF tertanam.
pub fn embedded_graph() -> CdResult<ModelGraph> {
    match datamodel() {
        Some(dm) => build_model_graph(dm),
        None => fail(Code::ScopeConfig, "dmmf.json tertanam tidak bisa dibaca."),
    }
}

/// `ScopeOwner`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// `{ kind: 'anchor', self: true }`.
    AnchorSelf,
    /// `{ kind: 'anchor', via: { model, column } }`.
    AnchorVia { model: String, column: String },
    /// `{ kind: 'scan' }`.
    Scan,
    /// `{ kind: 'fk', column, target }`.
    Fk { column: String, target: String },
}

/// `ScopePlanOptions`.
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_ephemeral: bool,
    pub skip_heavy: bool,
}

/// `ScopePlan` (himpunan berurutan seperti `Set` JS).
#[derive(Debug, Clone)]
pub struct ScopePlan {
    pub selected: IndexSet<String>,
    pub external: IndexSet<String>,
    pub hybrid: IndexSet<String>,
    pub uncovered: Vec<String>,
    pub owners: IndexMap<String, Owner>,
    pub order: Vec<String>,
    pub cycles: Vec<String>,
    pub scoped: IndexSet<String>,
}

fn model<'a>(models: &'a ModelGraph, name: &str) -> CdResult<&'a ModelInfo> {
    match models.get(name) {
        Some(m) => Ok(m),
        None => fail(Code::ScopeConfig, format!("Model tidak dikenal: {name}")),
    }
}

/// `buildScopePlan(models, opts)`.
pub fn build_scope_plan(models: &ModelGraph, opts: &PlanOptions) -> CdResult<ScopePlan> {
    let include: IndexSet<&str> = opts.include.iter().map(String::as_str).collect();
    let exclude: IndexSet<&str> = opts.exclude.iter().map(String::as_str).collect();
    for name in include.iter().chain(exclude.iter()) {
        if !models.contains_key(*name) {
            return fail(Code::InvalidParams, format!("Model tidak dikenal: {name}"));
        }
    }

    let external: IndexSet<String> = EXTERNAL_MODELS
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| models.contains_key(*n))
        .map(str::to_owned)
        .collect();
    let hybrid_set: IndexSet<String> = HYBRID_MODELS
        .iter()
        .map(|h| h.name)
        .filter(|n| models.contains_key(*n))
        .map(str::to_owned)
        .collect();

    for name in &hybrid_set {
        if external.contains(name) {
            return fail(
                Code::ScopeConfig,
                format!("{name} terdaftar sebagai global DAN separuh global. Pilih salah satu."),
            );
        }
        let info = model(models, name)?;
        if let Some(cfg) = hybrid(name) {
            for col in std::iter::once(&cfg.scope_column).chain(cfg.natural_key.iter()) {
                if !info.scalar_fields.iter().any(|f| f == col) {
                    return fail(
                        Code::ScopeConfig,
                        format!("HYBRID_MODELS menyebut {name}.{col} yang tidak ada di schema."),
                    );
                }
            }
        }
    }

    let mut scoped: IndexSet<String> = ANCHORS.iter().map(|a| a.name.to_owned()).collect();
    let mut grew = true;
    while grew {
        grew = false;
        for (name, info) in models {
            if scoped.contains(name) || external.contains(name) {
                continue;
            }
            let hit = info.fks.iter().any(|f| scoped.contains(&f.target))
                || info.list_fks.iter().any(|f| scoped.contains(&f.target))
                || info
                    .polymorphic
                    .is_some_and(|p| p.targets.iter().any(|(_, t)| scoped.contains(*t)));
            if hit {
                scoped.insert(name.clone());
                grew = true;
            }
        }
    }

    let uncovered: Vec<String> = models
        .keys()
        .filter(|n| !scoped.contains(*n) && !external.contains(*n))
        .cloned()
        .collect();

    let mut selected = scoped.clone();
    if !opts.include_ephemeral {
        for n in EPHEMERAL_MODELS {
            if !include.contains(n) {
                selected.shift_remove(*n);
            }
        }
    }
    if opts.skip_heavy {
        for n in HEAVY_MODELS {
            if !include.contains(n) {
                selected.shift_remove(*n);
            }
        }
    }
    for n in &exclude {
        if anchor(n).is_some() {
            return fail(
                Code::InvalidParams,
                format!("Model jangkar tidak boleh dikecualikan: {n}"),
            );
        }
        if hybrid_set.contains(*n) {
            return fail(
                Code::InvalidParams,
                format!(
                    "Model separuh global tidak boleh dikecualikan: {n}. Rujukan ke baris globalnya ikut hilang kalau ia dibuang."
                ),
            );
        }
        selected.shift_remove(*n);
    }

    let mut owners = IndexMap::new();
    for name in &selected {
        if let Some(a) = anchor(name) {
            let owner = match a.via {
                None => Owner::AnchorSelf,
                Some((model, column)) => Owner::AnchorVia {
                    model: model.to_owned(),
                    column: column.to_owned(),
                },
            };
            owners.insert(name.clone(), owner);
            continue;
        }
        let info = model(models, name)?;
        let candidates: Vec<&FkInfo> = info
            .fks
            .iter()
            .filter(|f| selected.contains(&f.target))
            .collect();
        let company_fk = candidates
            .iter()
            .find(|f| f.column == "companyId" && f.target == "company");
        let required = candidates.iter().find(|f| !f.nullable);
        let chosen = company_fk.or(required).or(candidates.first());
        let owner = match chosen {
            None => Owner::Scan,
            Some(f) => Owner::Fk {
                column: f.column.clone(),
                target: f.target.clone(),
            },
        };
        owners.insert(name.clone(), owner);
    }

    let (order, cycles) = topo_sort(models, &selected)?;

    Ok(ScopePlan {
        selected,
        external,
        hybrid: hybrid_set,
        uncovered,
        owners,
        order,
        cycles,
        scoped,
    })
}

/// `topoSort(models, selected)`.
fn topo_sort(
    models: &ModelGraph,
    selected: &IndexSet<String>,
) -> CdResult<(Vec<String>, Vec<String>)> {
    let mut deps: HashMap<String, HashSet<String>> = HashMap::new();
    for name in selected {
        let info = model(models, name)?;
        let mut targets = HashSet::new();
        for f in &info.fks {
            if selected.contains(&f.target) && f.target != *name {
                targets.insert(f.target.clone());
            }
        }
        if let Some(p) = info.polymorphic {
            for (_, t) in p.targets {
                if selected.contains(*t) && *t != name.as_str() {
                    targets.insert((*t).to_owned());
                }
            }
        }
        for f in &info.list_fks {
            if selected.contains(&f.target) && f.target != *name {
                targets.insert(f.target.clone());
            }
        }
        deps.insert(name.clone(), targets);
    }

    let mut order = Vec::new();
    let mut done: HashSet<String> = HashSet::new();
    let mut cycles = Vec::new();
    let mut remaining: Vec<String> = selected.iter().cloned().collect();

    while !remaining.is_empty() {
        let mut ready: Vec<String> = remaining
            .iter()
            .filter(|n| {
                deps.get(*n)
                    .is_none_or(|d| d.iter().all(|t| done.contains(t)))
            })
            .cloned()
            .collect();
        if ready.is_empty() {
            // Sort stabil menurun = elemen pertama dengan jumlah terbanyak.
            let mut best: Option<(&String, usize)> = None;
            for n in &remaining {
                let deferrable = model(models, n)?
                    .fks
                    .iter()
                    .filter(|f| f.nullable && !done.contains(&f.target))
                    .count();
                if best.is_none_or(|(_, b)| deferrable > b) {
                    best = Some((n, deferrable));
                }
            }
            let Some((cut, deferrable)) = best else {
                break;
            };
            if deferrable == 0 {
                return fail(
                    Code::ScopeConfig,
                    format!(
                        "Siklus foreign key tanpa kolom nullable untuk diputus: {}",
                        remaining.join(", ")
                    ),
                );
            }
            let cut = cut.clone();
            cycles.push(cut.clone());
            deps.insert(cut, HashSet::new());
            continue;
        }
        ready.sort();
        for n in &ready {
            order.push(n.clone());
            done.insert(n.clone());
        }
        remaining.retain(|n| !done.contains(n));
    }
    Ok((order, cycles))
}

/// `schemaFingerprint(models)`.
pub fn schema_fingerprint(models: &ModelGraph) -> String {
    let mut names: Vec<&String> = models.keys().collect();
    names.sort();
    let parts: Vec<String> = names
        .iter()
        .filter_map(|n| models.get(*n).map(|i| (n, i)))
        .map(|(name, info)| {
            let mut fields = info.scalar_fields.clone();
            fields.sort();
            format!("{name}:{}", fields.join(","))
        })
        .collect();
    let digest = Sha256::digest(parts.join("|").as_bytes());
    hex::encode(digest).chars().take(16).collect()
}

/// `describeDatabase(url)`: host + nama database saja (`host:port/db`).
pub fn describe_database(url: Option<&str>) -> Option<String> {
    let u = url::Url::parse(url?).ok()?;
    let host = u.host_str()?;
    if host.is_empty() {
        return None;
    }
    let port = u.port().unwrap_or(5432);
    Some(format!("{host}:{port}{}", u.path()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn golden() -> Value {
        serde_json::from_str(include_str!("../../tests/golden/scope_plan.json")).unwrap()
    }

    fn graph() -> ModelGraph {
        embedded_graph().unwrap()
    }

    fn owner_json(o: &Owner) -> Value {
        match o {
            Owner::AnchorSelf => json!({ "kind": "anchor", "self": true }),
            Owner::AnchorVia { model, column } => {
                json!({ "kind": "anchor", "via": { "model": model, "column": column } })
            }
            Owner::Scan => json!({ "kind": "scan" }),
            Owner::Fk { column, target } => {
                json!({ "kind": "fk", "column": column, "target": target })
            }
        }
    }

    fn strings(v: &Value) -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_owned())
            .collect()
    }

    /// Graf, rencana, dan sidik jari identik dengan keluaran Node (`tests/golden/scope_plan.json`,
    /// dibuat dari `company-scope.ts` asli dengan DMMF yang sama).
    #[test]
    fn identik_dengan_node() {
        let g = golden();
        let models = graph();
        assert_eq!(
            schema_fingerprint(&models),
            g["fingerprint"].as_str().unwrap()
        );

        for entry in g["models"].as_array().unwrap() {
            let name = entry[0].as_str().unwrap();
            let want = &entry[1];
            let info = &models[name];
            assert_eq!(info.table, want["table"].as_str().unwrap(), "{name}");
            assert_eq!(info.pk.as_deref(), want["pk"].as_str(), "{name}");
            assert_eq!(info.pk_has_default, want["pkHasDefault"].as_bool().unwrap());
            assert_eq!(info.uuid_col, want["uuidCol"].as_str() == Some("uuid"));
            let fks: Vec<Value> = info
                .fks
                .iter()
                .map(|f| {
                    json!({ "column": f.column, "target": f.target, "targetColumn": f.target_column,
                            "nullable": f.nullable, "declared": f.declared })
                })
                .collect();
            assert_eq!(Value::Array(fks), want["fks"], "{name}");
            assert_eq!(
                info.unique_columns,
                strings(&want["uniqueColumns"]),
                "{name}"
            );
            assert_eq!(info.scalar_fields, strings(&want["scalarFields"]), "{name}");
            let comp = want["compositePk"].as_array().map(|a| {
                a.iter()
                    .map(|s| s.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            });
            assert_eq!(info.composite_pk, comp, "{name}");
        }
        assert_eq!(models.len(), g["models"].as_array().unwrap().len());

        let cases: Vec<(&str, PlanOptions)> = vec![
            ("{}", PlanOptions::default()),
            (
                "{\"skipHeavy\":true}",
                PlanOptions {
                    skip_heavy: true,
                    ..PlanOptions::default()
                },
            ),
            (
                "{\"includeEphemeral\":true}",
                PlanOptions {
                    include_ephemeral: true,
                    ..PlanOptions::default()
                },
            ),
            (
                "{\"exclude\":[\"auditLog\"]}",
                PlanOptions {
                    exclude: vec!["auditLog".into()],
                    ..PlanOptions::default()
                },
            ),
        ];
        for (key, opts) in cases {
            let want = &g["plans"][key];
            let plan = build_scope_plan(&models, &opts).unwrap();
            assert_eq!(plan.order, strings(&want["order"]), "{key}");
            assert_eq!(plan.cycles, strings(&want["cycles"]), "{key}");
            assert_eq!(plan.uncovered, strings(&want["uncovered"]), "{key}");
            assert_eq!(
                plan.selected.iter().cloned().collect::<Vec<_>>(),
                strings(&want["selected"]),
                "{key}"
            );
            let owners: Vec<Value> = plan
                .owners
                .iter()
                .map(|(k, o)| json!([k, owner_json(o)]))
                .collect();
            assert_eq!(Value::Array(owners), want["owners"], "{key}");
            assert_eq!(
                super::super::backup::format_scope_plan(&plan),
                strings(&want["formatted"]),
                "{key}"
            );
        }
    }

    // ── company-scope.spec.ts ──

    #[test]
    fn graf_dari_dmmf_dengan_anchor_dan_uuid() {
        let models = graph();
        assert!(models.contains_key("company"));
        assert!(models["users"].uuid_col);
        let audit: Vec<String> = models["auditLog"]
            .fks
            .iter()
            .map(|f| format!("{}->{}", f.column, f.target))
            .collect();
        assert!(audit.contains(&"companyId->company".to_owned()));
        assert!(audit.contains(&"actorId->users".to_owned()));
        assert_eq!(
            models["auditLog"].polymorphic.map(|p| p.id_column),
            Some("entityId")
        );
    }

    #[test]
    fn urutan_topologis_induk_sebelum_anak() {
        let models = graph();
        let plan = build_scope_plan(&models, &PlanOptions::default()).unwrap();
        let pos: HashMap<&String, usize> =
            plan.order.iter().enumerate().map(|(i, n)| (n, i)).collect();
        let order_set: HashSet<&String> = plan.order.iter().collect();
        let selected_set: HashSet<&String> = plan.selected.iter().collect();
        assert_eq!(order_set, selected_set);
        for name in &plan.order {
            if plan.cycles.contains(name) {
                continue;
            }
            let info = &models[name];
            let targets = info
                .fks
                .iter()
                .map(|f| f.target.clone())
                .chain(info.list_fks.iter().map(|f| f.target.clone()))
                .chain(
                    info.polymorphic
                        .map(|p| {
                            p.targets
                                .iter()
                                .map(|(_, t)| (*t).to_owned())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                )
                .filter(|t| t != name && plan.selected.contains(t) && !plan.cycles.contains(t));
            for t in targets {
                assert!(pos[&t] < pos[name], "{t} sebelum {name}");
            }
        }
        assert_eq!(plan.owners["company"], Owner::AnchorSelf);
        assert_eq!(
            plan.owners["users"],
            Owner::AnchorVia {
                model: "userCompany".into(),
                column: "userId".into()
            }
        );
    }

    #[test]
    fn ephemeral_heavy_include() {
        let models = graph();
        let base = build_scope_plan(&models, &PlanOptions::default()).unwrap();
        for n in EPHEMERAL_MODELS {
            assert!(!base.selected.contains(*n));
        }
        let eph = build_scope_plan(
            &models,
            &PlanOptions {
                include_ephemeral: true,
                ..PlanOptions::default()
            },
        )
        .unwrap();
        assert!(eph.selected.contains("jwtBlackList"));
        let light = build_scope_plan(
            &models,
            &PlanOptions {
                skip_heavy: true,
                ..PlanOptions::default()
            },
        )
        .unwrap();
        for n in HEAVY_MODELS {
            assert!(!light.selected.contains(*n));
        }
        let forced = build_scope_plan(
            &models,
            &PlanOptions {
                skip_heavy: true,
                include: vec!["tracking".into()],
                ..PlanOptions::default()
            },
        )
        .unwrap();
        assert!(forced.selected.contains("tracking"));
    }

    fn code_of_plan(opts: PlanOptions) -> Option<Code> {
        build_scope_plan(&graph(), &opts)
            .err()
            .and_then(|e| super::super::errors::code_of(&e))
    }

    #[test]
    fn menolak_model_tak_dikenal_anchor_dan_hybrid() {
        assert_eq!(
            code_of_plan(PlanOptions {
                include: vec!["bukanModel".into()],
                ..PlanOptions::default()
            }),
            Some(Code::InvalidParams)
        );
        assert_eq!(
            code_of_plan(PlanOptions {
                exclude: vec!["users".into()],
                ..PlanOptions::default()
            }),
            Some(Code::InvalidParams)
        );
        assert_eq!(
            code_of_plan(PlanOptions {
                exclude: vec!["accessLevel".into()],
                ..PlanOptions::default()
            }),
            Some(Code::InvalidParams)
        );
    }

    fn field(name: &str, extra: Value) -> Value {
        let mut base = json!({ "name": name, "kind": "scalar", "type": "Int", "isRequired": true });
        if let (Value::Object(b), Value::Object(e)) = (&mut base, extra) {
            b.extend(e);
        }
        base
    }

    #[test]
    fn logical_fk_kolom_hilang_scope_config() {
        let dm: Datamodel = serde_json::from_value(json!({
            "enums": [],
            "models": [{ "name": "jwtBlackList", "fields": [field("id", json!({ "isId": true }))] }]
        }))
        .unwrap();
        let err = build_model_graph(&dm).unwrap_err().to_string();
        assert!(
            err.contains("LOGICAL_FKS menyebut jwtBlackList.userId"),
            "{err}"
        );
    }

    #[test]
    fn fk_gabungan_divisionmember_dan_penolakan() {
        let models = graph();
        let fks: Vec<String> = models["divisionMember"]
            .fks
            .iter()
            .map(|f| format!("{}->{}.{}", f.column, f.target, f.target_column))
            .collect();
        assert!(fks.contains(&"divisionId->division.id".to_owned()));
        assert!(fks.contains(&"companyId->company.id".to_owned()));

        let dm: Datamodel = serde_json::from_value(json!({
            "enums": [],
            "models": [
                { "name": "parent", "fields": [field("id", json!({ "isId": true })), field("a", json!({})), field("b", json!({}))] },
                { "name": "child", "fields": [
                    field("id", json!({ "isId": true })), field("a", json!({})), field("b", json!({})),
                    { "name": "parent", "kind": "object", "type": "parent",
                      "relationFromFields": ["a", "b"], "relationToFields": ["a", "b"] }
                ] }
            ]
        }))
        .unwrap();
        let err = build_model_graph(&dm).unwrap_err().to_string();
        assert!(
            err.contains("FK gabungan belum didukung: child.parent"),
            "{err}"
        );
    }

    #[test]
    fn fingerprint_dan_describe_database() {
        let fp = schema_fingerprint(&graph());
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(schema_fingerprint(&graph()), fp);
        assert_eq!(
            describe_database(Some(
                "postgresql://u:secret@db.example.com:25060/app?sslmode=require"
            ))
            .as_deref(),
            Some("db.example.com:25060/app")
        );
        assert_eq!(
            describe_database(Some("postgresql://u:p@localhost/app")).as_deref(),
            Some("localhost:5432/app")
        );
        assert_eq!(describe_database(None), None);
        assert_eq!(describe_database(Some("bukan url")), None);
    }

    #[test]
    fn known_partial_indexes_sama_dengan_apply_partial_indexes() {
        // Port `known-partial-indexes.spec.ts`: dibaca dari repo Node bila ada di sebelah.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../Mudah-API/prisma/apply-partial-indexes.js");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let mut names = Vec::new();
        let marker = "CREATE UNIQUE INDEX IF NOT EXISTS \"";
        let mut rest = text.as_str();
        while let Some(i) = rest.find(marker) {
            rest = &rest[i + marker.len()..];
            let Some(end) = rest.find('"') else { break };
            let name = &rest[..end];
            let tail = &rest[end..];
            let stmt_end = tail.find('`').unwrap_or(tail.len());
            if tail[..stmt_end].to_uppercase().contains("WHERE") {
                names.push(name.to_owned());
            }
        }
        names.sort();
        let mut known: Vec<String> = KNOWN_PARTIAL_UNIQUE_INDEXES
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        known.sort();
        assert!(names.len() > 20);
        assert_eq!(known, names);
    }
}
