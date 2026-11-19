//! Enum Postgres buatan Prisma (`"BaseRole"`, `"SubscriptionStatus"`, ...).
//!
//! Nama tipenya camelCase ber-kutip, dan sqlx me-resolve nama tipe custom tanpa
//! kutip (jadi huruf kecil) — sumber error "type does not exist" yang sunyi.
//! Karena itu enum di sini dipetakan sebagai TEXT:
//!
//! * baca:  `SELECT "baseRole"::text AS "baseRole"`
//! * tulis: `... = $1::"BaseRole"` dengan `$1` berupa string
//!
//! Makro [`text_enum!`] menurunkan `sqlx::Type/Encode/Decode` (sebagai text),
//! `Serialize/Deserialize` (sebagai string), `as_str`, dan `FromStr`.

#[macro_export]
macro_rules! text_enum {
    ($(#[$meta:meta])* $vis:vis enum $name:ident { $($variant:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[allow(non_camel_case_types)]
        $vis enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub fn as_str(&self) -> &'static str {
                match self { $($name::$variant => stringify!($variant)),+ }
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = ();
            fn from_str(s: &str) -> Result<Self, ()> {
                match s { $(stringify!($variant) => Ok($name::$variant),)+ _ => Err(()) }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = <::std::borrow::Cow<'de, str> as ::serde::Deserialize>::deserialize(d)?;
                raw.parse().map_err(|_| ::serde::de::Error::custom(format!(
                    "must be one of the following values: {}",
                    [$(stringify!($variant)),+].join(", ")
                )))
            }
        }

        impl ::sqlx::Type<::sqlx::Postgres> for $name {
            fn type_info() -> ::sqlx::postgres::PgTypeInfo {
                <&str as ::sqlx::Type<::sqlx::Postgres>>::type_info()
            }
            fn compatible(ty: &::sqlx::postgres::PgTypeInfo) -> bool {
                <&str as ::sqlx::Type<::sqlx::Postgres>>::compatible(ty)
            }
        }

        impl<'r> ::sqlx::Decode<'r, ::sqlx::Postgres> for $name {
            fn decode(value: ::sqlx::postgres::PgValueRef<'r>) -> Result<Self, ::sqlx::error::BoxDynError> {
                let raw = <&str as ::sqlx::Decode<::sqlx::Postgres>>::decode(value)?;
                raw.parse().map_err(|_| format!("unknown {} value: {raw}", stringify!($name)).into())
            }
        }

        impl<'q> ::sqlx::Encode<'q, ::sqlx::Postgres> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut ::sqlx::postgres::PgArgumentBuffer,
            ) -> Result<::sqlx::encode::IsNull, ::sqlx::error::BoxDynError> {
                <&str as ::sqlx::Encode<::sqlx::Postgres>>::encode_by_ref(&self.as_str(), buf)
            }
        }
    };
}

text_enum! {
    /// `roles/enums/role.enum.ts` — `accessLevel.code` role bawaan.
    pub enum Role { SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER, STAFF, GUEST }
}

text_enum! {
    /// Prisma `BaseRole` — plafon role custom.
    pub enum BaseRole { ADMIN, MANAGER, STAFF }
}

text_enum! {
    /// Prisma `PermissionAction`.
    pub enum PermissionAction { READ, WRITE, DELETE }
}

text_enum! {
    pub enum SubscriptionStatus { ACTIVE, INACTIVE, CANCELLED, PENDING, SUSPENDED, TRIAL }
}

text_enum! {
    pub enum SubscriptionBillingSource { PAYPAL, MANUAL }
}

text_enum! {
    pub enum PricingProductType { SALES, TIME, FULL }
}

text_enum! {
    /// `feature-modules/feature-module.enum.ts`.
    pub enum FeatureModule { TIME_TRACKING, SALES_TRACKER }
}

impl FeatureModule {
    pub fn label(&self) -> &'static str {
        match self {
            Self::TIME_TRACKING => "Time Tracking",
            Self::SALES_TRACKER => "Sales Tracker",
        }
    }
}

/// `permissions/role-rank.ts`. `None` untuk kode tak dikenal = SELALU tolak.
pub fn rank_of(code: &str) -> Option<u8> {
    match code {
        "SYSTEM_ADMIN" => Some(5),
        "SUPER_ADMIN" => Some(4),
        "ADMIN" => Some(3),
        "MANAGER" => Some(2),
        "STAFF" => Some(1),
        "GUEST" => Some(0),
        _ => None,
    }
}

/// `permissions/http-action.ts`. `None` = method tak dikenal = tolak.
pub fn action_for_method(method: &http::Method) -> Option<PermissionAction> {
    match method.as_str() {
        "GET" | "HEAD" | "OPTIONS" => Some(PermissionAction::READ),
        "POST" | "PATCH" | "PUT" => Some(PermissionAction::WRITE),
        "DELETE" => Some(PermissionAction::DELETE),
        _ => None,
    }
}

/// `effective-role.ts` `effectiveRoleCodeOf`.
pub fn effective_role_code(code: &str, is_system: bool, base_role: Option<BaseRole>) -> String {
    if is_system {
        return code.to_owned();
    }
    base_role.map_or_else(|| code.to_owned(), |b| b.as_str().to_owned())
}

/// `effective-role.ts` `COMPANY_WIDE_ROLE_CODES` / `seesWholeCompany`.
pub fn sees_whole_company(effective_role_code: Option<&str>) -> bool {
    matches!(
        effective_role_code,
        Some("SYSTEM_ADMIN" | "SUPER_ADMIN" | "ADMIN" | "MANAGER")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        assert_eq!("ADMIN".parse::<BaseRole>(), Ok(BaseRole::ADMIN));
        assert_eq!(
            serde_json::to_string(&Role::SUPER_ADMIN).ok().as_deref(),
            Some("\"SUPER_ADMIN\"")
        );
        assert!("admin".parse::<BaseRole>().is_err());
    }

    #[test]
    fn rank_unknown_is_none() {
        assert_eq!(rank_of("CUSTOM_X"), None);
        assert_eq!(rank_of("ADMIN"), Some(3));
    }
}
