-- GA4 server-side event tracking.
-- Semua DDL di sini juga terbentuk lewat `prisma db push` (tidak ada partial
-- index / trigger / CHECK), jadi tidak ada risiko hilang senyap di DB dev.

ALTER TABLE "users"
  ADD COLUMN "gaClientId" VARCHAR(64);

CREATE TABLE "analyticsEvent" (
  "id"         SERIAL       NOT NULL,
  "uuid"       UUID         NOT NULL DEFAULT gen_random_uuid(),
  "userId"     INTEGER      NOT NULL,
  "eventName"  VARCHAR(50)  NOT NULL,
  "status"     VARCHAR(20)  NOT NULL DEFAULT 'PENDING',
  "attempts"   INTEGER      NOT NULL DEFAULT 0,
  "params"     JSONB,
  "occurredAt" TIMESTAMP(3) NOT NULL,
  "sendingAt"  TIMESTAMP(3),
  "sentAt"     TIMESTAMP(3),
  "lastError"  TEXT,
  "createdAt"  TIMESTAMP(3) NOT NULL DEFAULT NOW(),
  "updatedAt"  TIMESTAMP(3) DEFAULT NOW(),

  CONSTRAINT "analyticsEvent_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "analyticsEvent_uuid_key" ON "analyticsEvent"("uuid");

-- Inti dari requirement "maksimal 1x per user seumur hidup". Dua pemicu yang
-- datang bersamaan sama-sama mencoba INSERT; persis satu menang, yang kalah
-- dapat P2002 dan diam.
CREATE UNIQUE INDEX "analyticsEvent_userId_eventName_key"
  ON "analyticsEvent"("userId", "eventName");

-- Sweeper retry: cari PENDING dengan attempts < N.
CREATE INDEX "analyticsEvent_status_attempts_idx"
  ON "analyticsEvent"("status", "attempts");

CREATE INDEX "analyticsEvent_eventName_createdAt_idx"
  ON "analyticsEvent"("eventName", "createdAt");

ALTER TABLE "analyticsEvent"
  ADD CONSTRAINT "analyticsEvent_userId_fkey"
  FOREIGN KEY ("userId") REFERENCES "users"("id")
  ON DELETE CASCADE ON UPDATE CASCADE;
