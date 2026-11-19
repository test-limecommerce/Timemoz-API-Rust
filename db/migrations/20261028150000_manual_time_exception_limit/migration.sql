-- Batas auto-approve manual time per karyawan (specs/005, FR-029..FR-036).
-- NULL = "Selalu butuh persetujuan" (arti lama, baris legacy tetap begini);
-- angka = batas khusus dalam menit pada periode company.
ALTER TABLE "manualTimeApprovalException" ADD COLUMN "limitMinutes" INTEGER;
