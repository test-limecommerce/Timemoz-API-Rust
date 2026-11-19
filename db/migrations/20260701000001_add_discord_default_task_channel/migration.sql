-- Add default task channel fields to company table
ALTER TABLE "company"
  ADD COLUMN IF NOT EXISTS "discordDefaultTaskChannelId"   VARCHAR(255),
  ADD COLUMN IF NOT EXISTS "discordDefaultTaskChannelName" VARCHAR(255);
