-- CreateEnum
CREATE TYPE "HolidayType" AS ENUM ('NATIONAL_HOLIDAY', 'JOINT_LEAVE');

-- CreateTable
CREATE TABLE "companyHoliday" (
    "id" SERIAL NOT NULL,
    "companyId" INTEGER NOT NULL,
    "name" VARCHAR(200) NOT NULL,
    "date" DATE NOT NULL,
    "type" "HolidayType" NOT NULL DEFAULT 'NATIONAL_HOLIDAY',
    "isActive" BOOLEAN NOT NULL DEFAULT true,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3),

    CONSTRAINT "companyHoliday_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE INDEX "companyHoliday_companyId_idx" ON "companyHoliday"("companyId");

-- CreateIndex
CREATE INDEX "companyHoliday_companyId_date_idx" ON "companyHoliday"("companyId", "date");

-- AddForeignKey
ALTER TABLE "companyHoliday" ADD CONSTRAINT "companyHoliday_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
