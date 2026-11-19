CREATE TABLE `leaveBalanceAdjustment` (
  `id`           INT           NOT NULL AUTO_INCREMENT,
  `balanceId`    INT           NOT NULL,
  `userId`       INT           NOT NULL,
  `companyId`    INT           NOT NULL,
  `adjustedById` INT           NOT NULL,
  `field`        VARCHAR(50)   NOT NULL,
  `delta`        DOUBLE        NOT NULL,
  `reason`       VARCHAR(500)  NOT NULL,
  `createdAt`    DATETIME(3)   NOT NULL DEFAULT NOW(),

  PRIMARY KEY (`id`),
  INDEX `leaveBalanceAdjustment_balanceId_idx` (`balanceId`),
  INDEX `leaveBalanceAdjustment_companyId_userId_idx` (`companyId`, `userId`),

  CONSTRAINT `leaveBalanceAdjustment_balanceId_fkey`
    FOREIGN KEY (`balanceId`) REFERENCES `leaveBalance` (`id`) ON DELETE CASCADE,
  CONSTRAINT `leaveBalanceAdjustment_userId_fkey`
    FOREIGN KEY (`userId`) REFERENCES `users` (`id`),
  CONSTRAINT `leaveBalanceAdjustment_companyId_fkey`
    FOREIGN KEY (`companyId`) REFERENCES `company` (`id`),
  CONSTRAINT `leaveBalanceAdjustment_adjustedById_fkey`
    FOREIGN KEY (`adjustedById`) REFERENCES `users` (`id`)
);
