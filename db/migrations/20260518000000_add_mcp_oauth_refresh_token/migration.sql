CREATE TABLE "mcpOAuthRefreshToken" (
  "id"          SERIAL PRIMARY KEY,
  "uuid"        UUID NOT NULL DEFAULT gen_random_uuid() UNIQUE,
  "tokenHash"   VARCHAR(255) NOT NULL UNIQUE,
  "tokenPrefix" VARCHAR(20) NOT NULL,
  "clientId"    UUID NOT NULL,
  "userId"      INTEGER NOT NULL,
  "scopes"      TEXT[] NOT NULL DEFAULT ARRAY['read'],
  "expiresAt"   TIMESTAMP(3) NOT NULL,
  "usedAt"      TIMESTAMP(3),
  "revokedAt"   TIMESTAMP(3),
  "createdAt"   TIMESTAMP(3) NOT NULL DEFAULT NOW(),
  "status"      INTEGER NOT NULL DEFAULT 1,
  CONSTRAINT "mcpOAuthRefreshToken_clientId_fkey"
    FOREIGN KEY ("clientId") REFERENCES "mcpOAuthClient"("id") ON DELETE CASCADE,
  CONSTRAINT "mcpOAuthRefreshToken_userId_fkey"
    FOREIGN KEY ("userId") REFERENCES "users"("id") ON DELETE CASCADE
);

CREATE INDEX "mcpOAuthRefreshToken_tokenHash_idx" ON "mcpOAuthRefreshToken"("tokenHash");
CREATE INDEX "mcpOAuthRefreshToken_userId_status_idx" ON "mcpOAuthRefreshToken"("userId", "status");
CREATE INDEX "mcpOAuthRefreshToken_clientId_userId_idx" ON "mcpOAuthRefreshToken"("clientId", "userId");
