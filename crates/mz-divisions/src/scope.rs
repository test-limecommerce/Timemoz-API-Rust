//! Jangkauan baca Leader (`division-scope.ts` `resolveViewerScope`).
//!
//! Seluruh langkah (role company, gerbang, kelayakan Leader, divisi + anggota) ada di
//! `mz_trackings::division::resolve_viewer_scope_with_divisions`; di sini hanya dipetakan ke `Scope`.

use mz_core::enums::sees_whole_company;
pub use mz_trackings::division::LedDivision as ScopeDivision;
use mz_trackings::division::{
    ViewerScope, is_leader_eligible_role_code, resolve_viewer_scope_with_divisions,
};
use sqlx::PgPool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Company,
    Division {
        /// Divisi hidup yang dipimpin viewer, urut nama lalu id; tidak pernah kosong.
        divisions: Vec<ScopeDivision>,
        /// Gabungan anggota hidup + aktif semua `divisions`, plus viewer; tanpa duplikat.
        user_ids: Vec<i32>,
    },
    SelfOnly,
}

impl Scope {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Company => "COMPANY",
            Self::Division { .. } => "DIVISION",
            Self::SelfOnly => "SELF",
        }
    }
}

/// `canLeadDivision`: setiap role efektif kecuali GUEST — jabatan company-wide atau yang
/// diterima `isLeaderEligibleRoleCode` (STAFF); `null` dan kode rusak ditolak.
pub fn can_lead_division(effective_role_code: Option<&str>) -> bool {
    sees_whole_company(effective_role_code) || is_leader_eligible_role_code(effective_role_code)
}

/// `resolveViewerScope` dengan daftar divisi yang dipimpin (urutan langkah: lihat
/// `mz_trackings::division::resolve_viewer_scope_with_divisions`).
pub async fn resolve_viewer_scope(db: &PgPool, viewer: i32, company: i32) -> sqlx::Result<Scope> {
    Ok(
        match resolve_viewer_scope_with_divisions(db, viewer, company).await? {
            (ViewerScope::Company, _) => Scope::Company,
            (ViewerScope::SelfOnly, _) => Scope::SelfOnly,
            (ViewerScope::Division { user_ids }, divisions) => Scope::Division {
                divisions,
                user_ids,
            },
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_can_lead() {
        for c in ["SYSTEM_ADMIN", "SUPER_ADMIN", "ADMIN", "MANAGER", "STAFF"] {
            assert!(can_lead_division(Some(c)), "{c}");
        }
        for c in [Some("GUEST"), Some("c1_custom"), Some(""), None] {
            assert!(!can_lead_division(c), "{c:?}");
        }
    }

    #[test]
    fn kinds() {
        assert_eq!(Scope::Company.kind(), "COMPANY");
        assert_eq!(Scope::SelfOnly.kind(), "SELF");
        assert_eq!(
            Scope::Division {
                divisions: vec![],
                user_ids: vec![]
            }
            .kind(),
            "DIVISION"
        );
    }
}
