-- AlterTable
ALTER TABLE "geofence"
  ADD COLUMN "timezone"         VARCHAR(100),
  ADD COLUMN "weeklySchedule"   JSONB,
  ADD COLUMN "breakTimeHours"   INTEGER NOT NULL DEFAULT 0,
  ADD COLUMN "breakTimeMinutes" INTEGER NOT NULL DEFAULT 0;
