CREATE TABLE "userKycFace" (
  "id"        SERIAL PRIMARY KEY,
  "userId"    INTEGER NOT NULL,
  "faces"     TEXT NOT NULL,
  "createdAt" TIMESTAMP NOT NULL DEFAULT NOW(),
  "updatedAt" TIMESTAMP DEFAULT NOW(),
  CONSTRAINT "userKycFace_userId_fkey" FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE
);

CREATE UNIQUE INDEX "userKycFace_userId_key" ON "userKycFace"("userId");
