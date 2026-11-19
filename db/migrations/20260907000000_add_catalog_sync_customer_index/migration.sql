-- docs/product_master.md F9, §10.2/§10.4.
-- `customer` predates this plan (docs/customers.md) and was never given the
-- (companyId, updatedAt, id) keyset index the other 13 F1 tables got, because F1 only
-- indexed the tables it created. F9's mobile catalog sync needs it for two reasons:
-- 1. the `assignments` delta stream (§0.33) is keyset-paginated on `customer.updatedAt`;
-- 2. the `catalogToken` aggregate (§10.2) takes MAX("updatedAt") over `customer` among
--    the other catalog tables, and needs that scan to be index-only.
CREATE INDEX "customer_companyId_updatedAt_id_idx" ON "customer"("companyId", "updatedAt", "id");
