//! Port `src/audit-logs/audit-scope.ts`: model mana yang diaudit, atas nama entitas apa,
//! dan dari mana `companyId`/label-nya.

use crate::row::{Row, as_text};

/// §0.23 — menyamai plafon `bulkGroup` (500 uuid sekali jalan).
pub const AUDIT_BULK_THRESHOLD: usize = 500;

/// `entityId` baris ringkasan massal (§3.5).
pub const AUDIT_BULK_ENTITY_ID: i32 = 0;

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
            pub fn from_name(s: &str) -> Option<Self> {
                match s { $($s => Some($name::$variant),)+ _ => None }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

str_enum!(
    /// Enum Postgres `"AuditEntityType"`.
    EntityType {
        Company => "company",
        Employee => "employee",
        Store => "store",
        StoreGroup => "storeGroup",
        StoreVisitForm => "storeVisitForm",
        StoreVisitPlan => "storeVisitPlan",
        Geofence => "geofence",
        NegativeZone => "negativeZone",
        Role => "role",
        Customer => "customer",
        CustomerType => "customerType",
        CustomerGroup => "customerGroup",
        CustomerChannel => "customerChannel",
        CustomerSubChannel => "customerSubChannel",
        Product => "product",
        ProductCategory => "productCategory",
        ProductSubCategory => "productSubCategory",
        ProductBrand => "productBrand",
        ProductPrincipal => "productPrincipal",
        Uom => "uom",
        TaxRate => "taxRate",
        PriceList => "priceList",
        PriceListItem => "priceListItem",
        ProductCustomField => "productCustomField",
        Order => "order",
        OrderType => "orderType",
        OrderReason => "orderReason",
        OrderPaymentTerm => "orderPaymentTerm",
        OrderPaymentMethod => "orderPaymentMethod",
        OrderCustomFieldDefinition => "orderCustomFieldDefinition",
        VisitType => "visitType",
        VisitFrequency => "visitFrequency",
        Division => "division",
    }
);

str_enum!(
    /// Enum Postgres `"AuditAction"`.
    Action {
        Create => "create",
        Update => "update",
        Delete => "delete",
        Restore => "restore",
    }
);

str_enum!(
    /// Enum Postgres `"AuditActorType"`.
    ActorType {
        User => "user",
        System => "system",
        Webhook => "webhook",
    }
);

str_enum!(
    /// Model Prisma yang diaudit (entitas §4.1 + pendamping §4.2). Nama = nama tabel.
    Model {
        Company => "company",
        Users => "users",
        Store => "store",
        StoreGroup => "storeGroup",
        StoreVisitFormTemplate => "storeVisitFormTemplate",
        StoreVisitPlan => "storeVisitPlan",
        Geofence => "geofence",
        NegativeZone => "negativeZone",
        AccessLevel => "accessLevel",
        Customer => "customer",
        CustomerType => "customerType",
        CustomerGroup => "customerGroup",
        CustomerChannel => "customerChannel",
        CustomerSubChannel => "customerSubChannel",
        Product => "product",
        ProductCategory => "productCategory",
        ProductSubCategory => "productSubCategory",
        ProductBrand => "productBrand",
        ProductPrincipal => "productPrincipal",
        Uom => "uom",
        TaxRate => "taxRate",
        ProductCustomField => "productCustomField",
        PriceList => "priceList",
        PriceListItem => "priceListItem",
        OrderType => "orderType",
        OrderReason => "orderReason",
        OrderPaymentTerm => "orderPaymentTerm",
        OrderPaymentMethod => "orderPaymentMethod",
        VisitType => "visitType",
        VisitFrequency => "visitFrequency",
        Division => "division",
        OrderCustomFieldDefinition => "orderCustomFieldDefinition",
        Order => "order",
        UserAccessLevel => "userAccessLevel",
        UserCompany => "userCompany",
        StoreGroupUser => "storeGroupUser",
        DivisionMember => "divisionMember",
        StoreVisitFormTemplateUser => "storeVisitFormTemplateUser",
        RolePermission => "rolePermission",
        StoreVisitFormReviser => "storeVisitFormReviser",
        ManualTimeApprovalException => "manualTimeApprovalException",
        CompanyWorkPattern => "companyWorkPattern",
        CompanyOvertimeSetting => "companyOvertimeSetting",
        OvertimeApprover => "overtimeApprover",
    }
);

/// `AuditEntityModelSpec`.
#[derive(Clone, Copy)]
pub struct EntitySpec {
    pub entity_type: EntityType,
    pub company_id_of: fn(&Row) -> Option<i64>,
    pub label_of: fn(&Row) -> Option<String>,
    /// `labelNeedsStoreCode` (`storeVisitPlan`).
    pub label_needs_store_code: bool,
    /// `restorable !== false`.
    pub restorable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionMode {
    Membership,
    Row,
}

/// `AuditMemberKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    User,
    UserHardDelete,
    Grant,
    UserRule,
    UserRole,
}

/// `AuditCompanionModelSpec`.
#[derive(Clone, Copy)]
pub struct CompanionSpec {
    pub entity_type: EntityType,
    pub parent_model: Model,
    pub parent_id_field: &'static str,
    pub sub_entity: &'static str,
    pub mode: CompanionMode,
    pub company_id_of: fn(&Row) -> Option<i64>,
    /// `memberKind` (default `user`).
    pub member_kind: MemberKind,
}

#[derive(Clone, Copy)]
pub enum Spec {
    Entity(EntitySpec),
    Companion(CompanionSpec),
}

impl Spec {
    pub fn entity_type(&self) -> EntityType {
        match self {
            Spec::Entity(e) => e.entity_type,
            Spec::Companion(c) => c.entity_type,
        }
    }
    pub fn company_id_of(&self, row: &Row) -> Option<i64> {
        match self {
            Spec::Entity(e) => (e.company_id_of)(row),
            Spec::Companion(c) => (c.company_id_of)(row),
        }
    }
}

fn own_company(row: &Row) -> Option<i64> {
    row.int("companyId")
}
fn id_as_company(row: &Row) -> Option<i64> {
    row.int("id")
}
fn no_company(_: &Row) -> Option<i64> {
    None
}
fn name_label(row: &Row) -> Option<String> {
    row.text("name").map(str::to_owned)
}
fn pair(a: Option<&str>, b: Option<&str>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a} — {b}")),
        (a, b) => a.or(b).map(str::to_owned),
    }
}
/// `code — name`.
fn code_and_name(row: &Row) -> Option<String> {
    pair(row.text("code"), row.text("name"))
}
/// `key — label`.
fn key_and_label(row: &Row) -> Option<String> {
    pair(row.text("key"), row.text("label"))
}
fn sku_and_name(row: &Row) -> Option<String> {
    pair(row.text("sku"), row.text("name"))
}
/// `asText(row.id)` — `id` selalu angka, jadi selalu `null` (sama dengan Node).
fn id_text_label(row: &Row) -> Option<String> {
    row.get("id").and_then(as_text).map(str::to_owned)
}
fn order_number_label(row: &Row) -> Option<String> {
    row.text("orderNumber").map(str::to_owned)
}
fn visit_date_label(row: &Row) -> Option<String> {
    row.get("visitDate").and_then(format_visit_date)
}

/// `formatVisitDate` — nilai tanggal di baris sudah berupa ISO string.
pub fn format_visit_date(value: &serde_json::Value) -> Option<String> {
    let s = value.as_str()?;
    Some(s.chars().take(10).collect())
}

const fn entity(
    entity_type: EntityType,
    company_id_of: fn(&Row) -> Option<i64>,
    label_of: fn(&Row) -> Option<String>,
    restorable: bool,
) -> Spec {
    Spec::Entity(EntitySpec {
        entity_type,
        company_id_of,
        label_of,
        label_needs_store_code: false,
        restorable,
    })
}

const fn companion(
    entity_type: EntityType,
    parent_model: Model,
    parent_id_field: &'static str,
    sub_entity: &'static str,
    mode: CompanionMode,
    member_kind: MemberKind,
    company_id_of: fn(&Row) -> Option<i64>,
) -> Spec {
    Spec::Companion(CompanionSpec {
        entity_type,
        parent_model,
        parent_id_field,
        sub_entity,
        mode,
        company_id_of,
        member_kind,
    })
}

impl Model {
    /// `AUDIT_ENTITY_MODELS[model] ?? AUDIT_COMPANION_MODELS[model]`.
    pub fn spec(self) -> Spec {
        use CompanionMode::{Membership, Row as RowMode};
        use EntityType as E;
        use MemberKind as K;
        match self {
            Model::Company => entity(E::Company, id_as_company, name_label, true),
            Model::Users => entity(E::Employee, no_company, name_label, true),
            Model::Store => entity(E::Store, own_company, code_and_name, true),
            Model::StoreGroup => entity(E::StoreGroup, own_company, code_and_name, true),
            Model::StoreVisitFormTemplate => {
                entity(E::StoreVisitForm, own_company, name_label, true)
            }
            Model::StoreVisitPlan => Spec::Entity(EntitySpec {
                entity_type: E::StoreVisitPlan,
                company_id_of: own_company,
                label_of: visit_date_label,
                label_needs_store_code: true,
                restorable: true,
            }),
            Model::Geofence => entity(E::Geofence, own_company, name_label, true),
            Model::NegativeZone => entity(E::NegativeZone, own_company, name_label, true),
            Model::AccessLevel => entity(E::Role, own_company, name_label, false),
            Model::Customer => entity(E::Customer, own_company, code_and_name, true),
            Model::CustomerType => entity(E::CustomerType, own_company, code_and_name, false),
            Model::CustomerGroup => entity(E::CustomerGroup, own_company, code_and_name, false),
            Model::CustomerChannel => entity(E::CustomerChannel, own_company, code_and_name, false),
            Model::CustomerSubChannel => {
                entity(E::CustomerSubChannel, own_company, code_and_name, false)
            }
            Model::Product => entity(E::Product, own_company, sku_and_name, true),
            Model::ProductCategory => entity(E::ProductCategory, own_company, code_and_name, false),
            Model::ProductSubCategory => {
                entity(E::ProductSubCategory, own_company, code_and_name, false)
            }
            Model::ProductBrand => entity(E::ProductBrand, own_company, code_and_name, false),
            Model::ProductPrincipal => {
                entity(E::ProductPrincipal, own_company, code_and_name, false)
            }
            Model::Uom => entity(E::Uom, own_company, code_and_name, false),
            Model::TaxRate => entity(E::TaxRate, own_company, code_and_name, false),
            Model::ProductCustomField => {
                entity(E::ProductCustomField, own_company, key_and_label, false)
            }
            Model::PriceList => entity(E::PriceList, own_company, code_and_name, true),
            Model::PriceListItem => entity(E::PriceListItem, own_company, id_text_label, false),
            Model::OrderType => entity(E::OrderType, own_company, code_and_name, false),
            Model::OrderReason => entity(E::OrderReason, own_company, code_and_name, false),
            Model::OrderPaymentTerm => {
                entity(E::OrderPaymentTerm, own_company, code_and_name, false)
            }
            Model::OrderPaymentMethod => {
                entity(E::OrderPaymentMethod, own_company, code_and_name, false)
            }
            Model::VisitType => entity(E::VisitType, own_company, code_and_name, false),
            Model::VisitFrequency => entity(E::VisitFrequency, own_company, code_and_name, false),
            Model::Division => entity(E::Division, own_company, name_label, false),
            Model::OrderCustomFieldDefinition => entity(
                E::OrderCustomFieldDefinition,
                own_company,
                key_and_label,
                false,
            ),
            Model::Order => entity(E::Order, own_company, order_number_label, false),

            Model::UserAccessLevel => companion(
                E::Employee,
                Model::Users,
                "userId",
                "accessLevel",
                RowMode,
                K::User,
                own_company,
            ),
            Model::UserCompany => companion(
                E::Employee,
                Model::Users,
                "userId",
                "companyMembership",
                RowMode,
                K::User,
                own_company,
            ),
            Model::StoreGroupUser => companion(
                E::StoreGroup,
                Model::StoreGroup,
                "groupId",
                "member",
                Membership,
                K::User,
                no_company,
            ),
            Model::DivisionMember => companion(
                E::Division,
                Model::Division,
                "divisionId",
                "divisionMember",
                Membership,
                K::UserRole,
                no_company,
            ),
            Model::StoreVisitFormTemplateUser => companion(
                E::StoreVisitForm,
                Model::StoreVisitFormTemplate,
                "templateId",
                "assignment",
                Membership,
                K::User,
                no_company,
            ),
            Model::RolePermission => companion(
                E::Role,
                Model::AccessLevel,
                "accessLevelId",
                "permission",
                Membership,
                K::Grant,
                no_company,
            ),
            Model::StoreVisitFormReviser => companion(
                E::Company,
                Model::Company,
                "companyId",
                "formReviser",
                Membership,
                K::User,
                own_company,
            ),
            Model::ManualTimeApprovalException => companion(
                E::Company,
                Model::Company,
                "companyId",
                "manualTimeApprovalException",
                Membership,
                K::UserRule,
                own_company,
            ),
            Model::CompanyWorkPattern => companion(
                E::Company,
                Model::Company,
                "companyId",
                "workPattern",
                RowMode,
                K::User,
                own_company,
            ),
            Model::CompanyOvertimeSetting => companion(
                E::Company,
                Model::Company,
                "companyId",
                "overtimeSetting",
                RowMode,
                K::User,
                own_company,
            ),
            Model::OvertimeApprover => companion(
                E::Company,
                Model::Company,
                "companyId",
                "overtimeApprover",
                Membership,
                K::UserHardDelete,
                own_company,
            ),
        }
    }

    pub fn entity_spec(self) -> Option<EntitySpec> {
        match self.spec() {
            Spec::Entity(e) => Some(e),
            Spec::Companion(_) => None,
        }
    }

    pub fn companion_spec(self) -> Option<CompanionSpec> {
        match self.spec() {
            Spec::Companion(c) => Some(c),
            Spec::Entity(_) => None,
        }
    }

    /// Tabel keanggotaan (`AuditCompanionModelSpec.mode === 'membership'`), mis.
    /// `storeGroupUser`, `divisionMember`, `rolePermission`, `overtimeApprover`.
    pub fn is_membership_companion(self) -> bool {
        self.companion_spec()
            .is_some_and(|c| c.mode == CompanionMode::Membership)
    }

    /// `INTERNAL_FIELDS[model] ?? ['id', 'uuid']` — kolom yang tetap dibaca pra-baca
    /// meski tidak ikut snapshot.
    pub fn internal_fields(self) -> &'static [&'static str] {
        match self {
            Model::Users => &["id", "uuid", "name"],
            Model::UserAccessLevel => &["id", "userId", "companyId"],
            Model::StoreGroupUser => &["id", "groupId", "userId", "deletedAt"],
            Model::DivisionMember => &["id", "divisionId", "userId", "role", "deletedAt"],
            Model::StoreVisitFormTemplateUser => &["id", "templateId", "userId", "deletedAt"],
            Model::RolePermission => &["id", "accessLevelId"],
            Model::StoreVisitFormReviser => &["id", "companyId", "userId", "deletedAt"],
            Model::ManualTimeApprovalException => {
                &["id", "companyId", "userId", "deletedAt", "limitMinutes"]
            }
            _ => &["id", "uuid"],
        }
    }

    /// `PARENT_SELECT[model]` (hanya untuk model yang bisa jadi induk).
    pub fn parent_select(self) -> &'static [&'static str] {
        match self {
            Model::Users | Model::Company => &["id", "uuid", "name"],
            Model::StoreGroup => &["id", "uuid", "companyId", "code", "name"],
            _ => &["id", "uuid", "companyId", "name"],
        }
    }
}

/// `parentLabelOf`.
pub fn parent_label_of(parent: Model, row: &Row) -> Option<String> {
    match parent.spec() {
        Spec::Entity(e) => (e.label_of)(row),
        Spec::Companion(_) => None,
    }
}

/// `parentCompanyIdOf`.
pub fn parent_company_id_of(parent: Model, row: &Row) -> Option<i64> {
    parent.spec().company_id_of(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_companions_match_restore_list() {
        let mut names: Vec<&str> = Model::ALL
            .iter()
            .filter(|m| m.is_membership_companion())
            .map(|m| m.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "divisionMember",
                "manualTimeApprovalException",
                "overtimeApprover",
                "rolePermission",
                "storeGroupUser",
                "storeVisitFormReviser",
                "storeVisitFormTemplateUser",
            ]
        );
    }

    #[test]
    fn labels_follow_node() {
        let store = Row::new().with("code", "A1").with("name", "Toko A");
        assert_eq!(
            Model::Store
                .entity_spec()
                .and_then(|s| (s.label_of)(&store)),
            Some("A1 — Toko A".into())
        );
        let only_name = Row::new().with("code", "").with("name", "Toko A");
        assert_eq!(code_and_name(&only_name), Some("Toko A".into()));
        assert_eq!(id_text_label(&Row::new().with("id", 5)), None);
        let plan = Row::new().with("visitDate", "2026-08-20T00:00:00.000Z");
        assert_eq!(visit_date_label(&plan), Some("2026-08-20".into()));
    }

    #[test]
    fn company_parent_uses_its_own_id() {
        let company = Row::new().with("id", 7).with("name", "Acme");
        assert_eq!(parent_company_id_of(Model::Company, &company), Some(7));
        assert_eq!(
            parent_company_id_of(Model::Users, &Row::new().with("id", 5)),
            None
        );
        assert_eq!(
            parent_label_of(Model::Users, &Row::new().with("name", "Budi")),
            Some("Budi".into())
        );
    }

    #[test]
    fn every_model_has_columns() {
        for m in Model::ALL {
            assert!(crate::read::columns_of(*m).is_some(), "{m}");
        }
        for e in EntityType::ALL {
            assert_eq!(EntityType::from_name(e.as_str()), Some(*e));
        }
    }
}
