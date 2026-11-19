//! `users/entities/register.entity.ts` — konstanta status `users.status`.

pub const STATUS_DELETED: i32 = 0;
pub const STATUS_ACTIVE: i32 = 1;
pub const STATUS_INVITED: i32 = 2;
pub const STATUS_DISABLED: i32 = 3;
pub const STATUS_UPDATED: i32 = 1;
/// Baris token reset password: 2 = sudah terpakai / dicabut.
pub const RESET_TOKEN_USED: i32 = 2;

/// `userCompany.status` / `userAccessLevel.status` / `accessLevel.status`: 1 = aktif.
pub const ACTIVE: i32 = 1;
