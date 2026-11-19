-- "invoices"."subscriptionId" adalah foreign key tanpa index (Postgres tidak membuatnya
-- otomatis). Lookup per subscription — cek invoice 24 jam terakhir di cron billing,
-- cek proration UNPAID saat invite, webhook PayPal — semuanya seq scan seluruh tabel.
-- Kolom invoiceType & status ikut karena hampir semua lookup memfilter keduanya.

-- CreateIndex
CREATE INDEX "invoices_subscriptionId_invoiceType_status_idx" ON "invoices"("subscriptionId", "invoiceType", "status");
