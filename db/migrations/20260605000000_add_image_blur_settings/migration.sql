-- AlterTable
ALTER TABLE "company" ADD COLUMN "imageBlurEnabled" BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE "company" ADD COLUMN "imageBlurRoles" TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[];
