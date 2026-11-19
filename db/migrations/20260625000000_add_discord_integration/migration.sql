-- Add Discord fields to company table
ALTER TABLE "company"
  ADD COLUMN IF NOT EXISTS "discordGuildId"   VARCHAR(255),
  ADD COLUMN IF NOT EXISTS "discordGuildName" VARCHAR(255);

-- A Discord guild may be linked to at most one company. Postgres treats NULLs as
-- distinct, so multiple disconnected companies (NULL guild) remain valid.
CREATE UNIQUE INDEX IF NOT EXISTS "company_discordGuildId_key"
  ON "company" ("discordGuildId");

-- Create projectDiscordMapping table
CREATE TABLE IF NOT EXISTS "projectDiscordMapping" (
  "id"                 SERIAL NOT NULL,
  "uuid"               UUID NOT NULL DEFAULT gen_random_uuid(),
  "projectId"          INTEGER NOT NULL,
  "companyId"          INTEGER NOT NULL,
  "discordChannelId"   VARCHAR(255) NOT NULL,
  "discordChannelName" VARCHAR(255) NOT NULL,
  "notificationTypes"  JSONB,
  "status"             INTEGER NOT NULL DEFAULT 1,
  "createdAt"          TIMESTAMP(3) NOT NULL DEFAULT NOW(),
  "updatedAt"          TIMESTAMP(3) DEFAULT NOW(),

  CONSTRAINT "projectDiscordMapping_pkey" PRIMARY KEY ("id"),
  CONSTRAINT "projectDiscordMapping_uuid_key" UNIQUE ("uuid"),
  CONSTRAINT "projectDiscordMapping_projectId_companyId_key" UNIQUE ("projectId", "companyId"),
  CONSTRAINT "projectDiscordMapping_projectId_fkey"
    FOREIGN KEY ("projectId") REFERENCES "project" ("id") ON DELETE CASCADE,
  CONSTRAINT "projectDiscordMapping_companyId_fkey"
    FOREIGN KEY ("companyId") REFERENCES "company" ("id") ON DELETE CASCADE
);

-- Create userDiscordMapping table
CREATE TABLE IF NOT EXISTS "userDiscordMapping" (
  "id"                 SERIAL NOT NULL,
  "uuid"               UUID NOT NULL DEFAULT gen_random_uuid(),
  "userId"             INTEGER NOT NULL,
  "companyId"          INTEGER NOT NULL,
  "discordUserId"      VARCHAR(255) NOT NULL,
  "discordUsername"    VARCHAR(255),
  "discordDisplayName" VARCHAR(255),
  "discordAvatarUrl"   TEXT,
  "status"             INTEGER NOT NULL DEFAULT 1,
  "createdAt"          TIMESTAMP(3) NOT NULL DEFAULT NOW(),
  "updatedAt"          TIMESTAMP(3) DEFAULT NOW(),

  CONSTRAINT "userDiscordMapping_pkey" PRIMARY KEY ("id"),
  CONSTRAINT "userDiscordMapping_uuid_key" UNIQUE ("uuid"),
  CONSTRAINT "userDiscordMapping_userId_companyId_key" UNIQUE ("userId", "companyId"),
  CONSTRAINT "userDiscordMapping_discordUserId_companyId_key" UNIQUE ("discordUserId", "companyId"),
  CONSTRAINT "userDiscordMapping_userId_fkey"
    FOREIGN KEY ("userId") REFERENCES "users" ("id") ON DELETE CASCADE,
  CONSTRAINT "userDiscordMapping_companyId_fkey"
    FOREIGN KEY ("companyId") REFERENCES "company" ("id") ON DELETE CASCADE
);
