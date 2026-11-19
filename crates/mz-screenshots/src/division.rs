//! Gerbang divisi (`divisions/division-scope.ts`, `division-gate.service.ts`,
//! `plan-entitlements.ts`): implementasinya di `mz_trackings::division`, dipakai bersama.

pub use mz_trackings::division::{
    ViewerScope, can_view_user, effective_code, resolve_viewer_scope, role_code_in_company,
    scope_forbidden,
};
