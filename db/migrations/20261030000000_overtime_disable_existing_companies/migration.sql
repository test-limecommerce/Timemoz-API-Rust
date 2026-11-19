-- specs/004-overtime-pp35 — lembur PP 35 NONAKTIF untuk company yang sudah ada saat
-- migration ini dijalankan. Company yang dibuat sesudahnya tetap tanpa baris dan
-- dibaca aktif (DEFAULT_OVERTIME_ENABLED = true, dibuat lazy oleh `ensureSettings`).
--
-- Satu statement (CTE data-modifying) supaya atomik tanpa bergantung pada transaksi
-- migrate. ON CONFLICT DO NOTHING: company yang barisnya sudah ada (sudah diatur
-- admin, atau sudah lewat `ensureSettings`) tidak disentuh — mematikan fitur yang
-- sudah berjalan wajib lewat PATCH pengaturan supaya pengajuan aktif ikut dibatalkan
-- (FR-034) dan tercatat di audit log.
--
-- Owner di-seed sebagai approver hanya untuk baris yang benar-benar di-INSERT di
-- sini, meniru `ensureSettings`: baris yang sudah ada membuat `ensureSettings`
-- langsung return, jadi tanpa seed ini daftar approver kosong saat admin
-- mengaktifkan fitur. Owner = penugasan SUPER_ADMIN bawaan aktif tertua milik user
-- aktif yang juga anggota aktif company itu (FR-005, T118).
WITH inserted AS (
    INSERT INTO "companyOvertimeSetting" ("companyId", "enabled", "retroWindowDays", "createdAt", "updatedAt")
    SELECT c."id", false, 7, now(), now()
    FROM "company" c
    ON CONFLICT ("companyId") DO NOTHING
    RETURNING "companyId"
),
owner AS (
    SELECT DISTINCT ON (ual."companyId") ual."companyId", ual."userId"
    FROM "userAccessLevel" ual
    JOIN inserted i ON i."companyId" = ual."companyId"
    JOIN "accessLevel" al ON al."id" = ual."accessLevelId"
    JOIN "users" u ON u."id" = ual."userId"
    WHERE ual."status" = 1
      AND al."isSystem" = true
      AND al."code" = 'SUPER_ADMIN'
      AND u."status" = 1
      AND EXISTS (
          SELECT 1 FROM "userCompany" uc
          WHERE uc."userId" = ual."userId"
            AND uc."companyId" = ual."companyId"
            AND uc."status" = 1
      )
    ORDER BY ual."companyId", ual."createdAt" ASC, ual."id" ASC
)
INSERT INTO "overtimeApprover" ("companyId", "userId", "createdAt", "createdById")
SELECT o."companyId", o."userId", now(), NULL
FROM owner o
ON CONFLICT ("companyId", "userId") DO NOTHING;
