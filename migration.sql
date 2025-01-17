CREATE SCHEMA banking_stage_results;

CREATE TABLE banking_stage_results.transaction_infos (
  signature CHAR(88),
  first_notification_slot BIGINT NOT NULL,
  errors text,
  is_executed BOOL,
  is_confirmed BOOL,
  cu_requested BIGINT,
  prioritization_fees BIGINT,
  utc_timestamp TIMESTAMP WITH TIME ZONE NOT NULL,
  accounts_used text,
  processed_slot BIGINT,
  supp_infos text,
  Primary key (signature, first_notification_slot)
);

CREATE TABLE banking_stage_results.blocks (
  slot BIGINT PRIMARY KEY,
  block_hash CHAR(44),
  leader_identity CHAR(44),
  successful_transactions BIGINT,
  banking_stage_errors BIGINT,
  processed_transactions BIGINT,
  total_cu_used BIGINT,
  total_cu_requested BIGINT,
  heavily_writelocked_accounts text,
  heavily_readlocked_accounts text,
  supp_infos text
);

CREATE INDEX idx_blocks_slot ON banking_stage_results.blocks(slot);
-- optional
CLUSTER banking_stage_results.blocks using idx_blocks_slot;
VACUUM FULL banking_stage_results.blocks;
CREATE INDEX idx_blocks_slot_errors ON banking_stage_results.blocks(slot) WHERE banking_stage_errors > 0;

CREATE INDEX idx_transaction_infos_timestamp ON banking_stage_results.transaction_infos(utc_timestamp);
-- optional
CLUSTER banking_stage_results.transaction_infos using idx_transaction_infos_timestamp;
VACUUM FULL banking_stage_results.transaction_infos;