-- AlterTable
ALTER TABLE "company"
  ADD COLUMN "storeVisitMinDurationMinutes" INTEGER NOT NULL DEFAULT 0,
  ADD COLUMN "storeVisitCountsAsAttendance" BOOLEAN NOT NULL DEFAULT false;
