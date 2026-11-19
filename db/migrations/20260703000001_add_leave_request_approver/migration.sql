ALTER TABLE "leaveRequest" ADD COLUMN "approverId" INTEGER;

ALTER TABLE "leaveRequest" ADD CONSTRAINT "leaveRequest_approverId_fkey" FOREIGN KEY ("approverId") REFERENCES "users"("id") ON DELETE SET NULL ON UPDATE CASCADE;

CREATE INDEX "leaveRequest_approverId_status_idx" ON "leaveRequest"("approverId", "status");
