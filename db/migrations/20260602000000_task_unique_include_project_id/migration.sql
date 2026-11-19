-- Allow 1 ClickUp folder to link to multiple Timemoz projects.
-- Tasks are now uniquely identified per project, not globally per company.
ALTER TABLE "task" DROP CONSTRAINT IF EXISTS "task_externalId_companyId_source_key";
ALTER TABLE "task" ADD CONSTRAINT "task_externalId_projectId_companyId_source_key"
  UNIQUE ("externalId", "projectId", "companyId", "source");
