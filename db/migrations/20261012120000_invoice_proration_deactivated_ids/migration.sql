-- AlterTable
ALTER TABLE "invoices" ADD COLUMN "prorationDeactivatedUserCompanyIds" INTEGER[] DEFAULT ARRAY[]::INTEGER[];
