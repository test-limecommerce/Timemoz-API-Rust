-- Allow googleCalendarEvent rows that are not backed by a real Google Calendar event
ALTER TABLE "googleCalendarEvent" ALTER COLUMN "googleEventId" DROP NOT NULL;

-- Track whether a row originated from Google or was created locally without Google integration
ALTER TABLE "googleCalendarEvent" ADD COLUMN "source" VARCHAR(20) NOT NULL DEFAULT 'google';
