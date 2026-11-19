-- Hapus company = hapus seluruh datanya. Sebelumnya 49 FK "companyId" ke
-- "company" memakai default Prisma untuk relasi wajib (ON DELETE RESTRICT),
-- sehingga DELETE company selalu tertahan begitu company punya store, order,
-- produk, cuti, dst.
--
-- "geofenceAuditLog" tidak punya "companyId"; ia ikut terhapus lewat geofence.
-- Tanpa CASCADE di FK geofence-nya, rantai company -> geofence tetap tertahan.
-- Kode aplikasi tidak pernah meng-hard-delete geofence.
--
-- FK Restrict ANTAR tabel anak (mis. storeVisit -> store, orderItem -> uom)
-- sengaja TIDAK diubah: Postgres menunda cek RI dari aksi cascade sampai akhir
-- statement, jadi DELETE company tetap lolos, sementara menghapus satu store /
-- uom yang masih dipakai tetap ditolak seperti sebelumnya.
--
-- NOT VALID + VALIDATE: data lama sudah valid terhadap FK yang sama, jadi
-- validasi dipisah agar ALTER TABLE tidak menahan lock berat selama scan.

ALTER TABLE "companyHoliday"
  DROP CONSTRAINT IF EXISTS "companyHoliday_companyId_fkey",
  ADD CONSTRAINT "companyHoliday_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "companyHoliday" VALIDATE CONSTRAINT "companyHoliday_companyId_fkey";

ALTER TABLE "companyLeaveType"
  DROP CONSTRAINT IF EXISTS "companyLeaveType_companyId_fkey",
  ADD CONSTRAINT "companyLeaveType_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "companyLeaveType" VALIDATE CONSTRAINT "companyLeaveType_companyId_fkey";

ALTER TABLE "customerChannel"
  DROP CONSTRAINT IF EXISTS "customerChannel_companyId_fkey",
  ADD CONSTRAINT "customerChannel_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "customerChannel" VALIDATE CONSTRAINT "customerChannel_companyId_fkey";

ALTER TABLE "customerGroup"
  DROP CONSTRAINT IF EXISTS "customerGroup_companyId_fkey",
  ADD CONSTRAINT "customerGroup_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "customerGroup" VALIDATE CONSTRAINT "customerGroup_companyId_fkey";

ALTER TABLE "customerSubChannel"
  DROP CONSTRAINT IF EXISTS "customerSubChannel_companyId_fkey",
  ADD CONSTRAINT "customerSubChannel_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "customerSubChannel" VALIDATE CONSTRAINT "customerSubChannel_companyId_fkey";

ALTER TABLE "customerType"
  DROP CONSTRAINT IF EXISTS "customerType_companyId_fkey",
  ADD CONSTRAINT "customerType_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "customerType" VALIDATE CONSTRAINT "customerType_companyId_fkey";

ALTER TABLE "dailyCompanySummary"
  DROP CONSTRAINT IF EXISTS "dailyCompanySummary_companyId_fkey",
  ADD CONSTRAINT "dailyCompanySummary_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "dailyCompanySummary" VALIDATE CONSTRAINT "dailyCompanySummary_companyId_fkey";

ALTER TABLE "employeeLeavePolicy"
  DROP CONSTRAINT IF EXISTS "employeeLeavePolicy_companyId_fkey",
  ADD CONSTRAINT "employeeLeavePolicy_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "employeeLeavePolicy" VALIDATE CONSTRAINT "employeeLeavePolicy_companyId_fkey";

ALTER TABLE "employeeLeaveTier"
  DROP CONSTRAINT IF EXISTS "employeeLeaveTier_companyId_fkey",
  ADD CONSTRAINT "employeeLeaveTier_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "employeeLeaveTier" VALIDATE CONSTRAINT "employeeLeaveTier_companyId_fkey";

ALTER TABLE "geofenceAuditLog"
  DROP CONSTRAINT IF EXISTS "geofenceAuditLog_geofenceId_fkey",
  ADD CONSTRAINT "geofenceAuditLog_geofenceId_fkey" FOREIGN KEY ("geofenceId") REFERENCES "geofence"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "geofenceAuditLog" VALIDATE CONSTRAINT "geofenceAuditLog_geofenceId_fkey";

ALTER TABLE "leaveBalance"
  DROP CONSTRAINT IF EXISTS "leaveBalance_companyId_fkey",
  ADD CONSTRAINT "leaveBalance_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "leaveBalance" VALIDATE CONSTRAINT "leaveBalance_companyId_fkey";

ALTER TABLE "leaveBalanceAdjustment"
  DROP CONSTRAINT IF EXISTS "leaveBalanceAdjustment_companyId_fkey",
  ADD CONSTRAINT "leaveBalanceAdjustment_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "leaveBalanceAdjustment" VALIDATE CONSTRAINT "leaveBalanceAdjustment_companyId_fkey";

ALTER TABLE "leavePolicy"
  DROP CONSTRAINT IF EXISTS "leavePolicy_companyId_fkey",
  ADD CONSTRAINT "leavePolicy_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "leavePolicy" VALIDATE CONSTRAINT "leavePolicy_companyId_fkey";

ALTER TABLE "leavePolicyApprover"
  DROP CONSTRAINT IF EXISTS "leavePolicyApprover_companyId_fkey",
  ADD CONSTRAINT "leavePolicyApprover_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "leavePolicyApprover" VALIDATE CONSTRAINT "leavePolicyApprover_companyId_fkey";

ALTER TABLE "leaveRequest"
  DROP CONSTRAINT IF EXISTS "leaveRequest_companyId_fkey",
  ADD CONSTRAINT "leaveRequest_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "leaveRequest" VALIDATE CONSTRAINT "leaveRequest_companyId_fkey";

ALTER TABLE "leaveTier"
  DROP CONSTRAINT IF EXISTS "leaveTier_companyId_fkey",
  ADD CONSTRAINT "leaveTier_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "leaveTier" VALIDATE CONSTRAINT "leaveTier_companyId_fkey";

ALTER TABLE "mobileBreakPolicy"
  DROP CONSTRAINT IF EXISTS "mobileBreakPolicy_companyId_fkey",
  ADD CONSTRAINT "mobileBreakPolicy_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "mobileBreakPolicy" VALIDATE CONSTRAINT "mobileBreakPolicy_companyId_fkey";

ALTER TABLE "mobileShift"
  DROP CONSTRAINT IF EXISTS "mobileShift_companyId_fkey",
  ADD CONSTRAINT "mobileShift_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "mobileShift" VALIDATE CONSTRAINT "mobileShift_companyId_fkey";

ALTER TABLE "negativeZone"
  DROP CONSTRAINT IF EXISTS "negativeZone_companyId_fkey",
  ADD CONSTRAINT "negativeZone_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "negativeZone" VALIDATE CONSTRAINT "negativeZone_companyId_fkey";

ALTER TABLE "order"
  DROP CONSTRAINT IF EXISTS "order_companyId_fkey",
  ADD CONSTRAINT "order_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "order" VALIDATE CONSTRAINT "order_companyId_fkey";

ALTER TABLE "orderCounter"
  DROP CONSTRAINT IF EXISTS "orderCounter_companyId_fkey",
  ADD CONSTRAINT "orderCounter_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "orderCounter" VALIDATE CONSTRAINT "orderCounter_companyId_fkey";

ALTER TABLE "orderCustomFieldDefinition"
  DROP CONSTRAINT IF EXISTS "orderCustomFieldDefinition_companyId_fkey",
  ADD CONSTRAINT "orderCustomFieldDefinition_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "orderCustomFieldDefinition" VALIDATE CONSTRAINT "orderCustomFieldDefinition_companyId_fkey";

ALTER TABLE "orderItem"
  DROP CONSTRAINT IF EXISTS "orderItem_companyId_fkey",
  ADD CONSTRAINT "orderItem_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "orderItem" VALIDATE CONSTRAINT "orderItem_companyId_fkey";

ALTER TABLE "orderPaymentMethod"
  DROP CONSTRAINT IF EXISTS "orderPaymentMethod_companyId_fkey",
  ADD CONSTRAINT "orderPaymentMethod_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "orderPaymentMethod" VALIDATE CONSTRAINT "orderPaymentMethod_companyId_fkey";

ALTER TABLE "orderPaymentTerm"
  DROP CONSTRAINT IF EXISTS "orderPaymentTerm_companyId_fkey",
  ADD CONSTRAINT "orderPaymentTerm_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "orderPaymentTerm" VALIDATE CONSTRAINT "orderPaymentTerm_companyId_fkey";

ALTER TABLE "orderReason"
  DROP CONSTRAINT IF EXISTS "orderReason_companyId_fkey",
  ADD CONSTRAINT "orderReason_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "orderReason" VALIDATE CONSTRAINT "orderReason_companyId_fkey";

ALTER TABLE "orderType"
  DROP CONSTRAINT IF EXISTS "orderType_companyId_fkey",
  ADD CONSTRAINT "orderType_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "orderType" VALIDATE CONSTRAINT "orderType_companyId_fkey";

ALTER TABLE "priceList"
  DROP CONSTRAINT IF EXISTS "priceList_companyId_fkey",
  ADD CONSTRAINT "priceList_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "priceList" VALIDATE CONSTRAINT "priceList_companyId_fkey";

ALTER TABLE "priceListItem"
  DROP CONSTRAINT IF EXISTS "priceListItem_companyId_fkey",
  ADD CONSTRAINT "priceListItem_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "priceListItem" VALIDATE CONSTRAINT "priceListItem_companyId_fkey";

ALTER TABLE "productBarcode"
  DROP CONSTRAINT IF EXISTS "productBarcode_companyId_fkey",
  ADD CONSTRAINT "productBarcode_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "productBarcode" VALIDATE CONSTRAINT "productBarcode_companyId_fkey";

ALTER TABLE "productBrand"
  DROP CONSTRAINT IF EXISTS "productBrand_companyId_fkey",
  ADD CONSTRAINT "productBrand_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "productBrand" VALIDATE CONSTRAINT "productBrand_companyId_fkey";

ALTER TABLE "productCategory"
  DROP CONSTRAINT IF EXISTS "productCategory_companyId_fkey",
  ADD CONSTRAINT "productCategory_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "productCategory" VALIDATE CONSTRAINT "productCategory_companyId_fkey";

ALTER TABLE "productCustomField"
  DROP CONSTRAINT IF EXISTS "productCustomField_companyId_fkey",
  ADD CONSTRAINT "productCustomField_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "productCustomField" VALIDATE CONSTRAINT "productCustomField_companyId_fkey";

ALTER TABLE "productImportJob"
  DROP CONSTRAINT IF EXISTS "productImportJob_companyId_fkey",
  ADD CONSTRAINT "productImportJob_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "productImportJob" VALIDATE CONSTRAINT "productImportJob_companyId_fkey";

ALTER TABLE "productPrincipal"
  DROP CONSTRAINT IF EXISTS "productPrincipal_companyId_fkey",
  ADD CONSTRAINT "productPrincipal_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "productPrincipal" VALIDATE CONSTRAINT "productPrincipal_companyId_fkey";

ALTER TABLE "productSubCategory"
  DROP CONSTRAINT IF EXISTS "productSubCategory_companyId_fkey",
  ADD CONSTRAINT "productSubCategory_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "productSubCategory" VALIDATE CONSTRAINT "productSubCategory_companyId_fkey";

ALTER TABLE "storeGroup"
  DROP CONSTRAINT IF EXISTS "storeGroup_companyId_fkey",
  ADD CONSTRAINT "storeGroup_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "storeGroup" VALIDATE CONSTRAINT "storeGroup_companyId_fkey";

ALTER TABLE "storeVisit"
  DROP CONSTRAINT IF EXISTS "storeVisit_companyId_fkey",
  ADD CONSTRAINT "storeVisit_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "storeVisit" VALIDATE CONSTRAINT "storeVisit_companyId_fkey";

ALTER TABLE "storeVisitAuditLog"
  DROP CONSTRAINT IF EXISTS "storeVisitAuditLog_companyId_fkey",
  ADD CONSTRAINT "storeVisitAuditLog_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "storeVisitAuditLog" VALIDATE CONSTRAINT "storeVisitAuditLog_companyId_fkey";

ALTER TABLE "storeVisitCounter"
  DROP CONSTRAINT IF EXISTS "storeVisitCounter_companyId_fkey",
  ADD CONSTRAINT "storeVisitCounter_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "storeVisitCounter" VALIDATE CONSTRAINT "storeVisitCounter_companyId_fkey";

ALTER TABLE "storeVisitNoOrder"
  DROP CONSTRAINT IF EXISTS "storeVisitNoOrder_companyId_fkey",
  ADD CONSTRAINT "storeVisitNoOrder_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "storeVisitNoOrder" VALIDATE CONSTRAINT "storeVisitNoOrder_companyId_fkey";

ALTER TABLE "storeVisitPlan"
  DROP CONSTRAINT IF EXISTS "storeVisitPlan_companyId_fkey",
  ADD CONSTRAINT "storeVisitPlan_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "storeVisitPlan" VALIDATE CONSTRAINT "storeVisitPlan_companyId_fkey";

ALTER TABLE "taxRate"
  DROP CONSTRAINT IF EXISTS "taxRate_companyId_fkey",
  ADD CONSTRAINT "taxRate_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "taxRate" VALIDATE CONSTRAINT "taxRate_companyId_fkey";

ALTER TABLE "visitFrequency"
  DROP CONSTRAINT IF EXISTS "visitFrequency_companyId_fkey",
  ADD CONSTRAINT "visitFrequency_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "visitFrequency" VALIDATE CONSTRAINT "visitFrequency_companyId_fkey";

ALTER TABLE "visitType"
  DROP CONSTRAINT IF EXISTS "visitType_companyId_fkey",
  ADD CONSTRAINT "visitType_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "visitType" VALIDATE CONSTRAINT "visitType_companyId_fkey";

ALTER TABLE "customer"
  DROP CONSTRAINT IF EXISTS "customer_companyId_fkey",
  ADD CONSTRAINT "customer_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "customer" VALIDATE CONSTRAINT "customer_companyId_fkey";

ALTER TABLE "geofence"
  DROP CONSTRAINT IF EXISTS "geofence_companyId_fkey",
  ADD CONSTRAINT "geofence_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "geofence" VALIDATE CONSTRAINT "geofence_companyId_fkey";

ALTER TABLE "product"
  DROP CONSTRAINT IF EXISTS "product_companyId_fkey",
  ADD CONSTRAINT "product_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "product" VALIDATE CONSTRAINT "product_companyId_fkey";

ALTER TABLE "store"
  DROP CONSTRAINT IF EXISTS "store_companyId_fkey",
  ADD CONSTRAINT "store_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "store" VALIDATE CONSTRAINT "store_companyId_fkey";

ALTER TABLE "uom"
  DROP CONSTRAINT IF EXISTS "uom_companyId_fkey",
  ADD CONSTRAINT "uom_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE NOT VALID;
ALTER TABLE "uom" VALIDATE CONSTRAINT "uom_companyId_fkey";
