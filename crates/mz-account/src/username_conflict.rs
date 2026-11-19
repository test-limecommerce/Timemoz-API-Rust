//! Port `src/users/username-conflict.ts`.

use mz_core::ApiError;

/// `isUsernameConflict` untuk `sqlx::Error`: pelanggaran unique pada `users.username`
/// (Prisma P2002 dengan target memuat `username`). Constraint Postgres-nya
/// `users_username_key`; nama kolom dicek juga untuk jaga-jaga.
pub fn is_username_conflict(err: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db) = err else {
        return false;
    };
    if !db.is_unique_violation() {
        return false;
    }
    constraint_mentions_username(db.constraint())
}

/// Versi untuk `ApiError` yang sudah dikonversi dari `sqlx::Error` (`UniqueViolation`).
pub fn is_username_conflict_api(err: &ApiError) -> bool {
    matches!(err, ApiError::UniqueViolation(msg) if msg.contains("username"))
}

fn constraint_mentions_username(constraint: Option<&str>) -> bool {
    constraint.is_some_and(|c| c.contains("username"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraint_matching() {
        assert!(constraint_mentions_username(Some("users_username_key")));
        assert!(!constraint_mentions_username(Some("users_googleId_key")));
        assert!(!constraint_mentions_username(None));
    }

    #[test]
    fn api_error_matching() {
        let hit = ApiError::UniqueViolation(
            "Unique constraint failed on the constraint: `users_username_key`".into(),
        );
        let miss = ApiError::UniqueViolation("`users_uuid_key`".into());
        assert!(is_username_conflict_api(&hit));
        assert!(!is_username_conflict_api(&miss));
        assert!(!is_username_conflict_api(&ApiError::bad_request("x")));
    }
}
