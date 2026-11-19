-- AlterTable
ALTER TABLE "project" ADD COLUMN "billable" BOOLEAN NOT NULL DEFAULT true;

-- AlterTable
ALTER TABLE "trackingTimeline" ADD COLUMN "billable" BOOLEAN;
