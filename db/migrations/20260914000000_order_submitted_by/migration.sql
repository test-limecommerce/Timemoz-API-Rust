-- Pelaku submit (docs dashboard orders-remaining-fixes-plan.md M6). Baris lama
-- tetap NULL: mengisinya dari "ownerId" justru mengarang pelaku yang belum
-- tentu benar.
ALTER TABLE "order" ADD COLUMN "submittedById" INTEGER;

ALTER TABLE "order"
  ADD CONSTRAINT "order_submittedById_fkey" FOREIGN KEY ("submittedById") REFERENCES "users"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
