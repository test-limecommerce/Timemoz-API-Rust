//! Scoping per role, builder filter daftar project, dan paginasi.

use mz_core::enums::sees_whole_company;
use mz_core::roles::role_codes;
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use crate::dto::PaginatorQuery;

/// `ProjectEntity.STATUS_ACTIVE`.
pub const STATUS_ACTIVE: i32 = 1;
/// `ProjectEntity.STATUS_DELETED`.
pub const STATUS_DELETED: i32 = 0;

/// `seesWholeCompany` atas salah satu role.
pub fn any_elevated(codes: &[String]) -> bool {
    codes.iter().any(|c| sees_whole_company(Some(c.as_str())))
}

pub async fn is_elevated<'e>(db: impl PgExecutor<'e>, user_id: i32) -> sqlx::Result<bool> {
    Ok(any_elevated(&role_codes(db, user_id).await?))
}

/// Batas data yang boleh dilihat pemanggil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Elevated dengan company aktif: project milik company.
    Company(i32),
    /// Selain itu: project yang ia ikuti.
    User(i32),
}

impl Scope {
    pub fn resolve(elevated: bool, company_id: Option<i32>, user_id: i32) -> Self {
        match (elevated, company_id) {
            (true, Some(c)) => Self::Company(c),
            _ => Self::User(user_id),
        }
    }
}

/// `WHERE` daftar project: `status = 1` + scope + `search` (contains insensitive
/// pada `code` ATAU `name`; Prisma tidak meng-escape `%`/`_`).
pub fn push_filter(qb: &mut QueryBuilder<'_, Postgres>, scope: Scope, search: Option<&str>) {
    qb.push("p.status = ").push_bind(STATUS_ACTIVE);
    match scope {
        Scope::Company(c) => {
            qb.push(r#" AND EXISTS (SELECT 1 FROM "projectCompany" pc WHERE pc."projectId" = p.id AND pc."companyId" = "#)
                .push_bind(c)
                .push(")");
        }
        Scope::User(u) => {
            qb.push(r#" AND EXISTS (SELECT 1 FROM "projectUser" pu WHERE pu."projectId" = p.id AND pu."userId" = "#)
                .push_bind(u)
                .push(")");
        }
    }
    if let Some(s) = search {
        let pattern = format!("%{s}%");
        qb.push(" AND (p.code ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR p.name ILIKE ")
            .push_bind(pattern)
            .push(")");
    }
}

/// Kolom skalar `project` yang boleh jadi `orderColumn` (Prisma menolak sisanya).
pub fn order_column(name: &str) -> Option<&'static str> {
    Some(match name {
        "id" => "p.id",
        "uuid" => "p.uuid",
        "code" => "p.code",
        "name" => "p.name",
        "screenshotPerHour" => "p.\"screenshotPerHour\"",
        "projectTypeId" => "p.\"projectTypeId\"",
        "startDate" => "p.\"startDate\"",
        "createdAt" => "p.\"createdAt\"",
        "updatedAt" => "p.\"updatedAt\"",
        "status" => "p.status",
        "totalHour" => "p.\"totalHour\"",
        "billable" => "p.billable",
        "clickupListId" => "p.\"clickupListId\"",
        _ => return None,
    })
}

/// Paginasi terselesaikan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paging {
    pub page: i64,
    pub per_page: i64,
    pub column: &'static str,
    pub desc: bool,
}

/// Paginasi yang belum tahu `total`: `per_page = None` berarti
/// `itemsPerPage = total` (semua baris).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LazyPaging {
    pub page: i64,
    pub per_page: Option<i64>,
    pub column: &'static str,
    pub desc: bool,
}

impl LazyPaging {
    pub fn with_total(&self, total: i64) -> Paging {
        Paging {
            page: self.page,
            per_page: self.per_page.unwrap_or(total),
            column: self.column,
            desc: self.desc,
        }
    }
}

/// Aturan default `findAll`: tanpa satu pun parameter paginasi/urutan → semua
/// baris, `createdAt desc`. Parameter yang tidak diisi saat sebagian diisi
/// diberi default (di Node: 500 NaN/undefined). `Err` = Prisma akan menolak.
///
/// Tidak butuh `total`, jadi validasi bisa jalan sebelum menyentuh DB.
pub fn resolve_paging_lazy(q: &PaginatorQuery) -> Result<LazyPaging, &'static str> {
    let page = q.page_number().unwrap_or(0);
    let per_page = q.items_per_page().unwrap_or(0);
    let by = q.order_by.as_deref().filter(|s| !s.is_empty());
    let col = q.order_column.as_deref().filter(|s| !s.is_empty());

    let (page, per_page, by, col) = if page == 0 && per_page == 0 && by.is_none() && col.is_none() {
        (1, None, "desc", "createdAt")
    } else {
        (
            if page == 0 { 1 } else { page },
            if per_page == 0 { None } else { Some(per_page) },
            by.unwrap_or("desc"),
            col.unwrap_or("createdAt"),
        )
    };
    let desc = match by {
        "desc" => true,
        "asc" => false,
        _ => return Err("Invalid orderBy"),
    };
    let column = order_column(col).ok_or("Invalid orderColumn")?;
    if page < 1 || per_page.is_some_and(|n| n < 0) {
        return Err("Invalid pagination");
    }
    Ok(LazyPaging {
        page,
        per_page,
        column,
        desc,
    })
}

/// [`resolve_paging_lazy`] dengan `total` sudah diketahui.
pub fn resolve_paging(q: &PaginatorQuery, total: i64) -> Result<Paging, &'static str> {
    resolve_paging_lazy(q).map(|p| p.with_total(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(
        page: Option<&str>,
        per: Option<&str>,
        by: Option<&str>,
        col: Option<&str>,
    ) -> PaginatorQuery {
        PaginatorQuery {
            page_number: page.map(Into::into),
            items_per_page: per.map(Into::into),
            order_by: by.map(Into::into),
            order_column: col.map(Into::into),
            search: None,
        }
    }

    #[test]
    fn defaults_return_everything_newest_first() {
        let p = resolve_paging(&q(None, None, None, None), 37).ok();
        assert_eq!(
            p,
            Some(Paging {
                page: 1,
                per_page: 37,
                column: "p.\"createdAt\"",
                desc: true
            })
        );
    }

    #[test]
    fn explicit_paging() {
        let p = resolve_paging(&q(Some("2"), Some("10"), Some("asc"), Some("name")), 99).ok();
        assert_eq!(
            p,
            Some(Paging {
                page: 2,
                per_page: 10,
                column: "p.name",
                desc: false
            })
        );
    }

    #[test]
    fn lazy_paging_leaves_per_page_open_until_total_known() {
        let p = resolve_paging_lazy(&q(None, None, None, None)).ok();
        assert_eq!(p.as_ref().map(|p| p.per_page), Some(None));
        assert_eq!(p.map(|p| p.with_total(12).per_page), Some(12));
        // `itemsPerPage=0` diperlakukan sama dengan tidak diisi.
        let p = resolve_paging_lazy(&q(Some("3"), Some("0"), None, None)).ok();
        assert_eq!(p.as_ref().map(|p| (p.page, p.per_page)), Some((3, None)));
        let p = resolve_paging_lazy(&q(Some("2"), Some("10"), None, None)).ok();
        assert_eq!(p.map(|p| p.per_page), Some(Some(10)));
        assert!(resolve_paging_lazy(&q(Some("1"), Some("-5"), None, None)).is_err());
    }

    #[test]
    fn rejects_unknown_column_and_direction() {
        assert!(resolve_paging(&q(Some("1"), Some("5"), Some("up"), None), 1).is_err());
        assert!(resolve_paging(&q(None, None, None, Some("password")), 1).is_err());
        assert!(resolve_paging(&q(Some("-1"), Some("5"), None, None), 1).is_err());
    }

    #[test]
    fn scope_resolution() {
        assert_eq!(Scope::resolve(true, Some(7), 1), Scope::Company(7));
        assert_eq!(Scope::resolve(true, None, 1), Scope::User(1));
        assert_eq!(Scope::resolve(false, Some(7), 1), Scope::User(1));
    }

    #[test]
    fn filter_sql_shapes() {
        let mut qb = QueryBuilder::<Postgres>::new("");
        push_filter(&mut qb, Scope::Company(3), Some("ab"));
        let sql = qb.sql().to_string();
        assert!(sql.contains("\"projectCompany\""));
        assert!(sql.contains("p.code ILIKE $3 OR p.name ILIKE $4"));

        let mut qb = QueryBuilder::<Postgres>::new("");
        push_filter(&mut qb, Scope::User(3), None);
        let sql = qb.sql().to_string();
        assert!(sql.contains("\"projectUser\""));
        assert!(!sql.contains("ILIKE"));
    }

    #[test]
    fn elevated_roles() {
        assert!(any_elevated(&["STAFF".into(), "MANAGER".into()]));
        assert!(!any_elevated(&[
            "STAFF".into(),
            "GUEST".into(),
            "c1_x".into()
        ]));
    }
}
