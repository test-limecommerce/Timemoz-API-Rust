-- Verifikasi "DB guard": partial/expression/GIN/INCLUDE index, CHECK constraint, dan trigger
-- yang tidak bisa dinyatakan schema.prisma. Port verifyDbGuards() dari Mudah-API
-- prisma/apply-partial-indexes.js (daftar INDEXES/CONSTRAINTS/TRIGGERS/OBSOLETE_INDEXES
-- di staging 1d581b8). Hanya MEMBACA katalog; tidak mengubah apa pun.
--
-- Keluaran: satu baris per masalah, kolom status|jenis|nama|alasan.
--   MISSING  guard tidak terpasang atau definisinya tidak sesuai -> perbaiki
--   OBSOLETE index usang masih ada -> DROP INDEX CONCURRENTLY
--   PENDING  tabel/kolom prasyarat belum ada (migrasi belum diterapkan) -> info
-- Kosong = semua guard terpasang.
--
-- Definisi guard ada di migrasi yang membuatnya (db/migrations). Migrasi baru yang
-- menambah/mengubah guard WAJIB menambah/mengubah barisnya di sini juga
-- (kolom: urutan, jenis, tabel, nama, potongan teks yang wajib ada di definisinya,
-- index unique?, tabel prasyarat, kolom prasyarat).
--
-- Jalankan: db/migrate.sh --verify-guards

WITH guard(ord, kind, tbl, name, expect, is_unique, req_table, req_columns) AS (VALUES
  (1, 'index', 'store', 'store_code_unique_per_company', '"deletedAt" IS NULL', true, 'store', ARRAY[]::text[]),
  (2, 'index', 'storeVisit', 'storeVisit_one_active_per_user', 'state = ''active''', true, 'storeVisit', ARRAY[]::text[]),
  (3, 'index', 'storeVisitPlan', 'storeVisitPlan_slot_unique_active', '"deletedAt" IS NULL', true, 'storeVisitPlan', ARRAY[]::text[]),
  (4, 'index', 'storeGroupUser', 'storeGroupUser_member_unique_active', '"deletedAt" IS NULL', true, 'storeGroupUser', ARRAY[]::text[]),
  (5, 'index', 'storeVisitFormTemplateUser', 'formTemplateUser_assignment_unique_active', '"deletedAt" IS NULL', true, 'storeVisitFormTemplateUser', ARRAY[]::text[]),
  (6, 'index', 'userCompany', 'userCompany_one_default_per_user', '"isDefault"', true, 'userCompany', ARRAY[]::text[]),
  (7, 'index', 'accessLevel', 'accessLevel_global_code_key', '"companyId" IS NULL', true, 'accessLevel', ARRAY['companyId']),
  (8, 'index', 'accessLevel', 'accessLevel_company_code_key', '"companyId" IS NOT NULL', true, 'accessLevel', ARRAY['companyId']),
  (9, 'index', 'accessLevel', 'accessLevel_company_override_key', 'status = 1', true, 'accessLevel', ARRAY['companyId', 'overridesRoleId']),
  (10, 'index', 'userAccessLevel', 'userAccessLevel_one_active_per_company', 'status = 1', true, 'userAccessLevel', ARRAY[]::text[]),
  (11, 'index', 'tableFieldVisibility', 'tableFieldVisibility_company_default_key', '"accessLevelId" IS NULL', true, 'tableFieldVisibility', ARRAY[]::text[]),
  (12, 'index', 'tableFieldVisibility', 'tableFieldVisibility_role_key', '"accessLevelId" IS NOT NULL', true, 'tableFieldVisibility', ARRAY[]::text[]),
  (13, 'index', 'storeVisitFormReviser', 'storeVisitFormReviser_unique_active', '"deletedAt" IS NULL', true, 'storeVisitFormReviser', ARRAY[]::text[]),
  (14, 'index', 'customer', 'customer_code_unique_per_company', '"deletedAt" IS NULL', true, 'customer', ARRAY[]::text[]),
  (15, 'index', 'product', 'product_sku_unique_per_company', '"deletedAt" IS NULL', true, 'product', ARRAY[]::text[]),
  (16, 'index', 'productUom', 'productUom_one_base_per_product', '"isBase"', true, 'productUom', ARRAY[]::text[]),
  (17, 'index', 'priceList', 'priceList_one_default_per_company', '"isDefault"', true, 'priceList', ARRAY[]::text[]),
  (18, 'index', 'priceListItem', 'priceListItem_tier_unique_active', '"deletedAt" IS NULL', true, 'priceListItem', ARRAY[]::text[]),
  (19, 'index', 'priceList', 'priceList_code_unique_per_company', '"deletedAt" IS NULL', true, 'priceList', ARRAY[]::text[]),
  (20, 'index', 'product', 'product_customFields_gin', 'jsonb_path_ops', false, 'product', ARRAY[]::text[]),
  (21, 'index', 'order', 'order_number_unique_per_company', '"orderNumber"', true, 'order', ARRAY[]::text[]),
  (22, 'index', 'order', 'order_client_request_unique', '"clientRequestId"', true, 'order', ARRAY[]::text[]),
  (23, 'index', 'orderType', 'orderType_code_unique_per_company', '"companyId", code', true, 'orderType', ARRAY[]::text[]),
  (24, 'index', 'orderType', 'orderType_one_default_per_company', '"isDefault"', true, 'orderType', ARRAY[]::text[]),
  (25, 'index', 'orderReason', 'orderReason_code_unique_per_company', '"reasonType", code', true, 'orderReason', ARRAY[]::text[]),
  (26, 'index', 'orderPaymentTerm', 'orderPaymentTerm_code_unique_per_company', '"companyId", code', true, 'orderPaymentTerm', ARRAY[]::text[]),
  (27, 'index', 'orderPaymentMethod', 'orderPaymentMethod_code_unique_per_company', '"companyId", code', true, 'orderPaymentMethod', ARRAY[]::text[]),
  (28, 'index', 'orderPaymentTerm', 'orderPaymentTerm_one_default_per_company', '"isDefault"', true, 'orderPaymentTerm', ARRAY[]::text[]),
  (29, 'index', 'orderPaymentMethod', 'orderPaymentMethod_one_default_per_company', '"isDefault"', true, 'orderPaymentMethod', ARRAY[]::text[]),
  (30, 'index', 'order', 'order_customFields_gin', 'jsonb_path_ops', false, 'order', ARRAY[]::text[]),
  (31, 'index', 'order', 'order_open_by_customer', '''draft''', false, 'order', ARRAY[]::text[]),
  (32, 'index', 'orderDelivery', 'orderDelivery_one_initial_per_order', '''initial''', true, 'orderDelivery', ARRAY[]::text[]),
  (33, 'index', 'visitType', 'visitType_one_default_per_company', '"isDefault"', true, 'visitType', ARRAY[]::text[]),
  (34, 'index', 'manualTimeApprovalException', 'manualTimeApprovalException_unique_active', '"deletedAt" IS NULL', true, 'manualTimeApprovalException', ARRAY[]::text[]),
  (35, 'index', 'app', 'app_timeline_usage_idx', 'INCLUDE', false, 'app', ARRAY[]::text[]),
  (36, 'index', 'overtimeRequest', 'overtimeRequest_one_active_per_day', 'PENDING_EMPLOYEE', true, 'overtimeRequest', ARRAY[]::text[]),
  (37, 'index', 'division', 'division_company_name_unique_active', 'lower((name)::text)', true, 'division', ARRAY[]::text[]),
  (38, 'index', 'divisionMember', 'divisionMember_division_user_unique_active', '"divisionId", "userId"', true, 'divisionMember', ARRAY[]::text[]),
  (39, 'index', 'invoices', 'invoices_manual_reference_key', '''manual''', true, 'invoices', ARRAY['paymentReference']),
  (40, 'index', 'adminJob', 'adminJob_one_active_per_company', 'RUNNING', true, 'adminJob', ARRAY['status']),
  (41, 'constraint', 'company', 'company_at_least_one_module', 'moduleTimeTrackingEnabled', true, 'company', ARRAY[]::text[]),
  (42, 'constraint', 'accessLevel', 'accessLevel_shape_chk', 'baseRole', true, 'accessLevel', ARRAY['companyId', 'isSystem', 'baseRole']),
  (43, 'constraint', 'accessLevel', 'accessLevel_system_always_active_chk', 'isSystem', true, 'accessLevel', ARRAY['isSystem']),
  (44, 'constraint', 'accessLevel', 'accessLevel_reserved_code_chk', 'SYSTEM_ADMIN', true, 'accessLevel', ARRAY['isSystem']),
  (45, 'constraint', 'productUom', 'productUom_conversion_positive_chk', 'conversionRate', true, 'productUom', ARRAY[]::text[]),
  (46, 'constraint', 'productUom', 'productUom_base_rate_one_chk', 'isBase', true, 'productUom', ARRAY[]::text[]),
  (47, 'constraint', 'priceListItem', 'priceListItem_qty_range_chk', 'minQty', true, 'priceListItem', ARRAY[]::text[]),
  (48, 'constraint', 'priceListItem', 'priceListItem_price_nonneg_chk', 'price', true, 'priceListItem', ARRAY[]::text[]),
  (49, 'constraint', 'product', 'product_customFields_object_chk', 'jsonb_typeof', true, 'product', ARRAY[]::text[]),
  (50, 'constraint', 'order', 'order_customFields_object_chk', 'jsonb_typeof', true, 'order', ARRAY[]::text[]),
  (51, 'constraint', 'orderItem', 'orderItem_customFields_object_chk', 'jsonb_typeof', true, 'orderItem', ARRAY[]::text[]),
  (52, 'constraint', 'orderItem', 'orderItem_qty_positive_chk', 'qty', true, 'orderItem', ARRAY[]::text[]),
  (53, 'constraint', 'orderItem', 'orderItem_discount_percent_chk', 'discountPercent', true, 'orderItem', ARRAY[]::text[]),
  (54, 'constraint', 'orderItem', 'orderItem_amounts_nonneg_chk', 'lineTotal', true, 'orderItem', ARRAY[]::text[]),
  (55, 'constraint', 'order', 'order_totals_nonneg_chk', 'grandTotal', true, 'order', ARRAY[]::text[]),
  (56, 'constraint', 'order', 'order_final_totals_nonneg_chk', 'finalGrandTotal', true, 'order', ARRAY['finalSubtotal', 'finalDiscountTotal', 'finalTaxTotal', 'finalGrandTotal']),
  (57, 'constraint', 'orderItem', 'orderItem_delivered_qty_chk', 'deliveredQty', true, 'orderItem', ARRAY['deliveredQty']),
  (58, 'constraint', 'orderItem', 'orderItem_final_amounts_nonneg_chk', 'finalLineTotal', true, 'orderItem', ARRAY['finalGrossAmount', 'finalDiscountValue', 'finalNetAmount', 'finalTaxAmount', 'finalLineTotal']),
  (59, 'constraint', 'orderDeliveryLine', 'orderDeliveryLine_qty_chk', 'qtyAfter', true, 'orderDeliveryLine', ARRAY[]::text[]),
  (60, 'constraint', 'visitFrequency', 'visitFrequency_interval_positive_chk', 'intervalDays', true, 'visitFrequency', ARRAY[]::text[]),
  (61, 'constraint', 'uom', 'uom_default_factor_positive_chk', 'defaultFactor', true, 'uom', ARRAY['defaultFactor']),
  (62, 'constraint', 'productUom', 'productUom_factor_positive_chk', 'factor', true, 'productUom', ARRAY['factor']),
  (63, 'constraint', 'productUom', 'productUom_base_factor_one_chk', 'operator', true, 'productUom', ARRAY['operator', 'factor']),
  (64, 'constraint', 'companyQuickstart', 'company_quickstart_finish_requires_steps', 'visitPlanAt', true, 'companyQuickstart', ARRAY[]::text[]),
  (65, 'constraint', 'companyWorkPattern', 'companyWorkPattern_shape_chk', 'shortDay', true, 'companyWorkPattern', ARRAY[]::text[]),
  (66, 'constraint', 'overtimeRequest', 'overtimeRequest_requested_sec_chk', 'requestedSec', true, 'overtimeRequest', ARRAY[]::text[]),
  (67, 'constraint', 'overtimeRequest', 'overtimeRequest_reject_reason_chk', 'rejectReason', true, 'overtimeRequest', ARRAY[]::text[]),
  (68, 'constraint', 'overtimeRequest', 'overtimeRequest_cancel_reason_chk', 'cancelReason', true, 'overtimeRequest', ARRAY[]::text[]),
  (69, 'constraint', 'overtimeRequest', 'overtimeRequest_overtime_sec_chk', 'overtimeSec', true, 'overtimeRequest', ARRAY[]::text[]),
  (70, 'trigger', 'subscriptions', 'subscriptions_paid_enables_activity_track', 'enable_activity_track_on_paid_subscription', true, 'company', ARRAY['useActivityTrack']),
  (71, 'trigger', 'accessLevel', 'accessLevel_no_delete_system', 'guard_system_access_level', true, 'accessLevel', ARRAY['isSystem']),
  (72, 'trigger', 'productCategory', 'productCategory_touch_updated_at', 'touch_updated_at', true, 'productCategory', ARRAY[]::text[]),
  (73, 'trigger', 'productSubCategory', 'productSubCategory_touch_updated_at', 'touch_updated_at', true, 'productSubCategory', ARRAY[]::text[]),
  (74, 'trigger', 'productBrand', 'productBrand_touch_updated_at', 'touch_updated_at', true, 'productBrand', ARRAY[]::text[]),
  (75, 'trigger', 'productPrincipal', 'productPrincipal_touch_updated_at', 'touch_updated_at', true, 'productPrincipal', ARRAY[]::text[]),
  (76, 'trigger', 'uom', 'uom_touch_updated_at', 'touch_updated_at', true, 'uom', ARRAY[]::text[]),
  (77, 'trigger', 'taxRate', 'taxRate_touch_updated_at', 'touch_updated_at', true, 'taxRate', ARRAY[]::text[]),
  (78, 'trigger', 'productCustomField', 'productCustomField_touch_updated_at', 'touch_updated_at', true, 'productCustomField', ARRAY[]::text[]),
  (79, 'trigger', 'product', 'product_touch_updated_at', 'touch_updated_at', true, 'product', ARRAY[]::text[]),
  (80, 'trigger', 'productUom', 'productUom_touch_updated_at', 'touch_updated_at', true, 'productUom', ARRAY[]::text[]),
  (81, 'trigger', 'productBarcode', 'productBarcode_touch_updated_at', 'touch_updated_at', true, 'productBarcode', ARRAY[]::text[]),
  (82, 'trigger', 'priceList', 'priceList_touch_updated_at', 'touch_updated_at', true, 'priceList', ARRAY[]::text[]),
  (83, 'trigger', 'priceListItem', 'priceListItem_touch_updated_at', 'touch_updated_at', true, 'priceListItem', ARRAY[]::text[]),
  (84, 'trigger', 'customer', 'customer_touch_updated_at', 'touch_updated_at', true, 'customer', ARRAY[]::text[]),
  (85, 'trigger', 'orderType', 'orderType_touch_updated_at', 'touch_updated_at', true, 'orderType', ARRAY[]::text[]),
  (86, 'trigger', 'orderReason', 'orderReason_touch_updated_at', 'touch_updated_at', true, 'orderReason', ARRAY[]::text[]),
  (87, 'trigger', 'orderPaymentTerm', 'orderPaymentTerm_touch_updated_at', 'touch_updated_at', true, 'orderPaymentTerm', ARRAY[]::text[]),
  (88, 'trigger', 'orderPaymentMethod', 'orderPaymentMethod_touch_updated_at', 'touch_updated_at', true, 'orderPaymentMethod', ARRAY[]::text[]),
  (89, 'trigger', 'order', 'order_touch_updated_at', 'touch_updated_at', true, 'order', ARRAY[]::text[]),
  (90, 'trigger', 'orderItem', 'orderItem_touch_updated_at', 'touch_updated_at', true, 'orderItem', ARRAY[]::text[]),
  (91, 'trigger', 'productUom', 'productUom_bump_product_ins', 'bump_product_updated_at', true, 'productUom', ARRAY[]::text[]),
  (92, 'trigger', 'productUom', 'productUom_bump_product_upd', 'bump_product_updated_at', true, 'productUom', ARRAY[]::text[]),
  (93, 'trigger', 'productUom', 'productUom_bump_product_del', 'bump_product_updated_at', true, 'productUom', ARRAY[]::text[]),
  (94, 'trigger', 'productBarcode', 'productBarcode_bump_product_ins', 'bump_product_updated_at', true, 'productBarcode', ARRAY[]::text[]),
  (95, 'trigger', 'productBarcode', 'productBarcode_bump_product_upd', 'bump_product_updated_at', true, 'productBarcode', ARRAY[]::text[]),
  (96, 'trigger', 'productBarcode', 'productBarcode_bump_product_del', 'bump_product_updated_at', true, 'productBarcode', ARRAY[]::text[]),
  (97, 'trigger', 'orderItem', 'orderItem_bump_order_ins', 'bump_order_updated_at', true, 'orderItem', ARRAY[]::text[]),
  (98, 'trigger', 'orderItem', 'orderItem_bump_order_upd', 'bump_order_updated_at', true, 'orderItem', ARRAY[]::text[]),
  (99, 'trigger', 'orderItem', 'orderItem_bump_order_del', 'bump_order_updated_at', true, 'orderItem', ARRAY[]::text[]),
  (100, 'obsolete', 'divisionMember', 'divisionMember_company_user_unique_active', NULL, true, 'divisionMember', ARRAY[]::text[]),
  (101, 'obsolete', 'divisionMember', 'divisionMember_one_leader_active', NULL, true, 'divisionMember', ARRAY[]::text[])
),
state AS (
  SELECT g.*,
         to_regclass(quote_ident(g.tbl)) IS NOT NULL AS has_table,
         (SELECT r.c FROM unnest(g.req_columns) WITH ORDINALITY AS r(c, i)
           WHERE NOT EXISTS (
             SELECT 1 FROM information_schema.columns ic
              WHERE ic.table_schema = current_schema()
                AND ic.table_name = g.req_table AND ic.column_name = r.c)
           ORDER BY r.i LIMIT 1) AS missing_column,
         CASE g.kind
           WHEN 'constraint' THEN (
             SELECT pg_get_constraintdef(oid) FROM pg_constraint
              WHERE conname = g.name AND connamespace = current_schema()::regnamespace LIMIT 1)
           WHEN 'trigger' THEN (
             SELECT pg_get_triggerdef(oid) FROM pg_trigger
              WHERE tgname = g.name AND NOT tgisinternal LIMIT 1)
           ELSE (
             SELECT indexdef FROM pg_indexes
              WHERE schemaname = current_schema() AND indexname = g.name)
         END AS def
    FROM guard g
),
verdict AS (
  SELECT ord, kind, name,
         CASE
           WHEN kind = 'obsolete' THEN CASE WHEN def IS NOT NULL THEN 'OBSOLETE' END
           WHEN NOT has_table OR missing_column IS NOT NULL THEN 'PENDING'
           WHEN def IS NULL THEN 'MISSING'
           WHEN kind = 'index'
                AND ((is_unique AND strpos(def, 'UNIQUE INDEX') = 0) OR strpos(def, expect) = 0)
             THEN 'MISSING'
           WHEN kind = 'constraint' AND (strpos(def, 'CHECK') = 0 OR strpos(def, expect) = 0)
             THEN 'MISSING'
           WHEN kind = 'trigger' AND strpos(def, expect) = 0 THEN 'MISSING'
         END AS status,
         CASE
           WHEN kind = 'obsolete' THEN 'index usang masih ada di tabel "' || tbl || '"'
           WHEN NOT has_table THEN 'tabel "' || tbl || '" belum ada'
           WHEN missing_column IS NOT NULL THEN 'kolom "' || missing_column || '" belum ada'
           WHEN def IS NULL THEN 'tidak terpasang'
           ELSE 'definisinya tidak sesuai: ' || def
         END AS reason
    FROM state
)
SELECT status, kind, name, reason FROM verdict WHERE status IS NOT NULL ORDER BY ord;
