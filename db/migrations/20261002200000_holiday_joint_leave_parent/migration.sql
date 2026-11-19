-- Cuti bersama terikat ke libur induknya (mis. Idul Fitri -> 20, 23, 24 Mar).
-- Baris lama tetap NULL dan diperlakukan sebagai libur / cuti bersama mandiri.
ALTER TABLE "companyHoliday" ADD COLUMN "parentHolidayId" INTEGER;

CREATE INDEX "companyHoliday_parentHolidayId_idx" ON "companyHoliday"("parentHolidayId");

ALTER TABLE "companyHoliday"
  ADD CONSTRAINT "companyHoliday_parentHolidayId_fkey"
  FOREIGN KEY ("parentHolidayId") REFERENCES "companyHoliday"("id")
  ON DELETE CASCADE ON UPDATE CASCADE;
