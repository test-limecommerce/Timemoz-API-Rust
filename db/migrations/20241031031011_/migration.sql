-- AlterTable
ALTER TABLE "accessLevel" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "client" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "company" ALTER COLUMN "dailyReportSentAt" SET DEFAULT TIME '00:00:00',
ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "companyClient" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "jwtBlackList" ALTER COLUMN "revokedAt" SET DEFAULT NOW(),
ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "project" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "projectClient" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "projectCompany" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "projectHour" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "projectType" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "projectUser" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "screenshot" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "setting" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "tracking" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "trackingTimeline" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "userAccessLevel" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "userCompany" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "userConnected" ALTER COLUMN "connectedAt" SET DEFAULT NOW(),
ALTER COLUMN "lastConnectedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "userForgotPassword" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "sentAt" SET DEFAULT NOW(),
ALTER COLUMN "expiredAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW();

-- AlterTable
ALTER TABLE "users" ALTER COLUMN "createdAt" SET DEFAULT NOW(),
ALTER COLUMN "updatedAt" SET DEFAULT NOW(),
ALTER COLUMN "sentAt" SET DEFAULT NOW();

-- CreateTable
CREATE TABLE "shareReport" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "name" VARCHAR(200) NOT NULL,
    "startAt" TIMESTAMP(3) NOT NULL,
    "endAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "shareReport_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "shareReportUsers" (
    "id" SERIAL NOT NULL,
    "shareId" INTEGER NOT NULL,
    "userId" INTEGER NOT NULL,

    CONSTRAINT "shareReportUsers_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "shareReportProjects" (
    "id" SERIAL NOT NULL,
    "shareId" INTEGER NOT NULL,
    "projectId" INTEGER NOT NULL,

    CONSTRAINT "shareReportProjects_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "shareReport_uuid_key" ON "shareReport"("uuid");

-- AddForeignKey
ALTER TABLE "shareReportUsers" ADD CONSTRAINT "shareReportUsers_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "shareReportUsers" ADD CONSTRAINT "shareReportUsers_shareId_fkey" FOREIGN KEY ("shareId") REFERENCES "shareReport"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "shareReportProjects" ADD CONSTRAINT "shareReportProjects_projectId_fkey" FOREIGN KEY ("projectId") REFERENCES "project"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "shareReportProjects" ADD CONSTRAINT "shareReportProjects_shareId_fkey" FOREIGN KEY ("shareId") REFERENCES "shareReport"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
