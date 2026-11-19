-- CreateTable
CREATE TABLE "users" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "username" VARCHAR(255) NOT NULL,
    "password" VARCHAR(255) NOT NULL,
    "name" VARCHAR(255) NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "sentAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "users_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "jwtBlackList" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "userId" INTEGER NOT NULL,
    "jwt" TEXT NOT NULL,
    "revokedAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "jwtBlackList_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "userForgotPassword" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "sentAt" TIMESTAMP(3) DEFAULT NOW(),
    "expiredAt" TIMESTAMP(3) DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "userId" INTEGER NOT NULL,
    "status" INTEGER NOT NULL,

    CONSTRAINT "userForgotPassword_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "accessLevel" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "code" VARCHAR(255) NOT NULL,
    "name" VARCHAR(255) NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "accessLevel_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "userAccessLevel" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "userId" INTEGER NOT NULL,
    "accessLevelId" INTEGER NOT NULL,
    "companyId" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "userAccessLevel_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "company" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "name" VARCHAR(255) NOT NULL,
    "telp" VARCHAR(255),
    "timeZone" VARCHAR(255) NOT NULL DEFAULT 'UTC',
    "dailyReportSentAt" TIME NOT NULL DEFAULT TIME '00:00:00',
    "isAlllowDeleteScreenshot" BOOLEAN NOT NULL DEFAULT false,
    "fixedHourNotification" INTEGER NOT NULL DEFAULT 4,
    "isAgencyMode" BOOLEAN NOT NULL DEFAULT false,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,
    "actualProfilePictureName" TEXT,
    "profilePictureFullPath" TEXT,

    CONSTRAINT "company_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "userCompany" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "userId" INTEGER NOT NULL,
    "companyId" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,
    "isScreenshotActive" BOOLEAN NOT NULL DEFAULT true,
    "isAllowManualTime" BOOLEAN NOT NULL DEFAULT false,
    "isBlur" BOOLEAN NOT NULL DEFAULT false,
    "isAllowEditTime" BOOLEAN NOT NULL DEFAULT false,
    "screenshotPerHour" INTEGER NOT NULL DEFAULT 3,
    "idleTimeInSecond" INTEGER NOT NULL,
    "isDefault" BOOLEAN NOT NULL,

    CONSTRAINT "userCompany_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "client" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "code" VARCHAR(255) NOT NULL,
    "name" VARCHAR(255) NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "client_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "companyClient" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "clientId" INTEGER NOT NULL,
    "companyId" INTEGER NOT NULL,
    "status" INTEGER NOT NULL,

    CONSTRAINT "companyClient_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "projectType" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "code" VARCHAR(255) NOT NULL,
    "name" VARCHAR(255) NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "projectType_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "project" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "code" VARCHAR(255) NOT NULL,
    "name" VARCHAR(255) NOT NULL,
    "screenshotPerHour" INTEGER NOT NULL,
    "projectTypeId" INTEGER NOT NULL,
    "startDate" DATE NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "project_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "projectClient" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "projectId" INTEGER NOT NULL,
    "clientId" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "projectClient_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "projectUser" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "projectId" INTEGER NOT NULL,
    "userId" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "projectUser_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "projectCompany" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "projectId" INTEGER NOT NULL,
    "companyId" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "projectCompany_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "tracking" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "takenDate" DATE NOT NULL,
    "isAbsent" BOOLEAN,
    "userId" INTEGER NOT NULL,
    "totalTakenInSecond" INTEGER,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "tracking_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "trackingTimeline" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "note" VARCHAR(255) NOT NULL,
    "isOffline" BOOLEAN NOT NULL DEFAULT false,
    "startAt" TIMESTAMP(3) NOT NULL,
    "endAt" TIMESTAMP(3) NOT NULL,
    "trackingId" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "trackingTimeline_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "screenshot" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "fileFullpath" VARCHAR(255) NOT NULL,
    "isBlur" BOOLEAN NOT NULL DEFAULT false,
    "takenAt" TIMESTAMP(3) NOT NULL,
    "trackingTimelineId" INTEGER NOT NULL,
    "projectId" INTEGER NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "screenshot_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "setting" (
    "id" SERIAL NOT NULL,
    "uuid" UUID NOT NULL DEFAULT gen_random_uuid(),
    "code" VARCHAR(255) NOT NULL,
    "value" TEXT,
    "label" VARCHAR(255),
    "description" TEXT,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT NOW(),
    "updatedAt" TIMESTAMP(3) DEFAULT NOW(),
    "status" INTEGER NOT NULL,

    CONSTRAINT "setting_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "users_uuid_key" ON "users"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "users_username_key" ON "users"("username");

-- CreateIndex
CREATE UNIQUE INDEX "jwtBlackList_uuid_key" ON "jwtBlackList"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "userForgotPassword_uuid_key" ON "userForgotPassword"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "accessLevel_uuid_key" ON "accessLevel"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "userAccessLevel_uuid_key" ON "userAccessLevel"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "company_uuid_key" ON "company"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "userCompany_uuid_key" ON "userCompany"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "client_uuid_key" ON "client"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "companyClient_uuid_key" ON "companyClient"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "projectType_uuid_key" ON "projectType"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "project_uuid_key" ON "project"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "projectClient_uuid_key" ON "projectClient"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "projectUser_uuid_key" ON "projectUser"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "projectCompany_uuid_key" ON "projectCompany"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "tracking_uuid_key" ON "tracking"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "tracking_userId_takenDate_key" ON "tracking"("userId", "takenDate");

-- CreateIndex
CREATE UNIQUE INDEX "trackingTimeline_uuid_key" ON "trackingTimeline"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "screenshot_uuid_key" ON "screenshot"("uuid");

-- CreateIndex
CREATE UNIQUE INDEX "setting_uuid_key" ON "setting"("uuid");

-- AddForeignKey
ALTER TABLE "userAccessLevel" ADD CONSTRAINT "userAccessLevel_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "userAccessLevel" ADD CONSTRAINT "userAccessLevel_accessLevelId_fkey" FOREIGN KEY ("accessLevelId") REFERENCES "accessLevel"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "userAccessLevel" ADD CONSTRAINT "userAccessLevel_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "userCompany" ADD CONSTRAINT "userCompany_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "userCompany" ADD CONSTRAINT "userCompany_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "companyClient" ADD CONSTRAINT "companyClient_clientId_fkey" FOREIGN KEY ("clientId") REFERENCES "client"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "companyClient" ADD CONSTRAINT "companyClient_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "project" ADD CONSTRAINT "project_projectTypeId_fkey" FOREIGN KEY ("projectTypeId") REFERENCES "projectType"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "projectClient" ADD CONSTRAINT "projectClient_projectId_fkey" FOREIGN KEY ("projectId") REFERENCES "project"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "projectClient" ADD CONSTRAINT "projectClient_clientId_fkey" FOREIGN KEY ("clientId") REFERENCES "client"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "projectUser" ADD CONSTRAINT "projectUser_projectId_fkey" FOREIGN KEY ("projectId") REFERENCES "project"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "projectUser" ADD CONSTRAINT "projectUser_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "projectCompany" ADD CONSTRAINT "projectCompany_projectId_fkey" FOREIGN KEY ("projectId") REFERENCES "project"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "projectCompany" ADD CONSTRAINT "projectCompany_companyId_fkey" FOREIGN KEY ("companyId") REFERENCES "company"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "tracking" ADD CONSTRAINT "tracking_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "trackingTimeline" ADD CONSTRAINT "trackingTimeline_trackingId_fkey" FOREIGN KEY ("trackingId") REFERENCES "tracking"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "screenshot" ADD CONSTRAINT "screenshot_trackingTimelineId_fkey" FOREIGN KEY ("trackingTimelineId") REFERENCES "trackingTimeline"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "screenshot" ADD CONSTRAINT "screenshot_projectId_fkey" FOREIGN KEY ("projectId") REFERENCES "project"("id") ON DELETE CASCADE ON UPDATE CASCADE;
