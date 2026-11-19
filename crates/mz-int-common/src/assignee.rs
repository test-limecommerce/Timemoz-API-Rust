//! Fragmen SQL bersama filter `assignee` (task ClickUp/Jira/integrations).
//!
//! `task.assignees` (jsonb) berisi array objek `{externalUserId, username, email, ...}`.
//! Filter cocok bila ada elemen yang `username` ATAU `email`-nya memuat teks (tanpa
//! membedakan huruf besar/kecil). `strpos` dipakai (bukan LIKE) agar `%`/`_` bukan wildcard.

use sqlx::{Postgres, QueryBuilder};

/// Menambahkan ` AND EXISTS (...)` pada `qb`; kolom `assignees` tak ber-alias.
pub fn push_assignee_filter(qb: &mut QueryBuilder<'_, Postgres>, needle: &str) {
    qb.push(
        " AND EXISTS (SELECT 1 FROM jsonb_array_elements(CASE WHEN jsonb_typeof(assignees) = 'array' \
         THEN assignees ELSE '[]'::jsonb END) AS a(e) WHERE strpos(lower(a.e->>'username'), lower(",
    )
    .push_bind(needle.to_owned())
    .push(")) > 0 OR strpos(lower(a.e->>'email'), lower(")
    .push_bind(needle.to_owned())
    .push(")) > 0)");
}
