//! Port `src/divisions/divisions.constants.ts`.

use mz_core::enums::Role;

pub const DIVISION_NOT_LEADER: &str = "DIVISION_NOT_LEADER";
pub use mz_trackings::division::DIVISION_SCOPE_FORBIDDEN;
pub const DIVISION_NOT_FOUND: &str = "DIVISION_NOT_FOUND";
pub const DIVISION_MEMBER_NOT_FOUND: &str = "DIVISION_MEMBER_NOT_FOUND";
pub const DIVISION_NAME_TAKEN: &str = "DIVISION_NAME_TAKEN";
pub const DIVISION_STALE: &str = "DIVISION_STALE";
pub const DIVISION_LEADER_REQUIRED: &str = "DIVISION_LEADER_REQUIRED";
pub const DIVISION_MEMBER_NOT_IN_COMPANY: &str = "DIVISION_MEMBER_NOT_IN_COMPANY";
pub const DIVISION_DUPLICATE_MEMBER: &str = "DIVISION_DUPLICATE_MEMBER";
pub const DIVISION_REPORT_RANGE_INVALID: &str = "DIVISION_REPORT_RANGE_INVALID";
pub const DIVISION_LEADER_NOT_ELIGIBLE: &str = "DIVISION_LEADER_NOT_ELIGIBLE";
pub const MODULE_DISABLED: &str = "MODULE_DISABLED";

pub const DIVISION_NAME_MAX: usize = 60;
pub const DIVISION_DESCRIPTION_MAX: usize = 200;
pub const DIVISION_MEMBERS_MAX: usize = 1000;
pub const DIVISION_REPORT_MAX_DAYS: i64 = 366;
pub const DIVISION_SEARCH_MAX: usize = 60;
pub const DIVISION_EMPLOYEES_PAGE_MAX: i64 = 500;

/// Jendela status online dan idle (menit).
pub const ONLINE_WINDOW_MINUTES: i64 = 10;
pub const IDLE_WINDOW_MINUTES: i64 = 60;

/// `users.status` dan `userAccessLevel.status`/`accessLevel.status`: 1 = aktif.
pub const ACTIVE: i32 = 1;

/// `membersPreview` memuat paling banyak 4 orang.
pub const MEMBERS_PREVIEW_MAX: usize = 4;

/// `READ_ROLES`.
pub const READ_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
    Role::GUEST,
];

/// `VIEW_ROLES`.
pub const VIEW_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// `WRITE_ROLES` (MANAGER sengaja tidak termasuk).
pub const WRITE_ROLES: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];

/// `WRITE_ROLES.includes(code)`.
pub fn is_write_role(code: &str) -> bool {
    WRITE_ROLES.iter().any(|r| r.as_str() == code)
}
