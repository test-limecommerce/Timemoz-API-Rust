-- ID sesi dari server untuk aturan satu device desktop. Token baru membawa `sid` di JWT dan
-- dicek terhadap kolom ini, bukan lagi MAC dari header (MAC bisa berubah: randomisasi Wi-Fi,
-- VPN, dock). Nullable: baris yang sudah ada tetap NULL dan token lamanya tetap dicek lewat MAC,
-- jadi migration ini tidak me-logout siapa pun. Jalankan SEBELUM deploy kode yang membacanya.

-- AlterTable
ALTER TABLE "userConnected" ADD COLUMN "sid" UUID;

-- CreateIndex
CREATE UNIQUE INDEX "userConnected_sid_key" ON "userConnected"("sid");
