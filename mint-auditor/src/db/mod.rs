// Copyright (c) 2018-2022 The MobileCoin Foundation

//! LMDB database abstraction.

mod block_audit_data;
mod block_balance;
mod counters;
mod models;
mod schema;
#[cfg(test)]
pub mod test_utils;

use crate::Error;
use diesel::{
    connection::SimpleConnection,
    prelude::*,
    r2d2::{ConnectionManager, Pool, PooledConnection},
    SqliteConnection,
};
use diesel_migrations::embed_migrations;
use mc_account_keys::burn_address_view_private;
use mc_common::{
    logger::{log, Logger},
    HashMap,
};
use mc_ledger_db::{Error as LedgerDbError, Ledger, LedgerDB};
use mc_transaction_core::{Block, BlockContents, BlockIndex, TokenId};
use std::{thread::sleep, time::Duration};

pub use block_audit_data::{BlockAuditData, BlockAuditDataModel};
pub use block_balance::{BlockBalance, BlockBalanceModel};
pub use counters::{Counters, CountersModel};

embed_migrations!("migrations/");

pub type Conn = PooledConnection<ConnectionManager<SqliteConnection>>;

#[derive(Debug)]
pub struct ConnectionOptions {
    pub enable_wal: bool,
    pub busy_timeout: Option<Duration>,
}

impl diesel::r2d2::CustomizeConnection<SqliteConnection, diesel::r2d2::Error>
    for ConnectionOptions
{
    fn on_acquire(&self, conn: &mut SqliteConnection) -> Result<(), diesel::r2d2::Error> {
        (|| {
            if let Some(d) = self.busy_timeout {
                conn.batch_execute(&format!("PRAGMA busy_timeout = {};", d.as_millis()))?;
            }
            if self.enable_wal {
                conn.batch_execute("
                    PRAGMA journal_mode = WAL;          -- better write-concurrency
                    PRAGMA synchronous = NORMAL;        -- fsync only in critical moments
                    PRAGMA wal_autocheckpoint = 1000;   -- write WAL changes back every 1000 pages, for an in average 1MB WAL file. May affect readers if number is increased
                    PRAGMA wal_checkpoint(TRUNCATE);    -- free some space by truncating possibly massive WAL files from the last run.
                ")?;
            }
            conn.batch_execute("PRAGMA foreign_keys = ON;")?;

            Ok(())
        })()
        .map_err(diesel::r2d2::Error::QueryError)
    }
}

#[derive(Clone)]
pub struct MintAuditorDb {
    pool: Pool<ConnectionManager<SqliteConnection>>,
    logger: Logger,
}

impl MintAuditorDb {
    pub fn new(pool: Pool<ConnectionManager<SqliteConnection>>, logger: Logger) -> Self {
        Self { pool, logger }
    }

    pub fn new_from_url(
        database_url: &str,
        db_connections: u32,
        logger: Logger,
    ) -> Result<Self, Error> {
        let manager = ConnectionManager::<SqliteConnection>::new(database_url);
        let pool = Pool::builder()
            .max_size(db_connections)
            .connection_customizer(Box::new(ConnectionOptions {
                enable_wal: true,
                busy_timeout: Some(Duration::from_secs(30)),
            }))
            .test_on_check_out(true)
            .build(manager)?;

        let conn = pool.get()?;
        embedded_migrations::run_with_output(&conn, &mut std::io::stdout())?;

        Ok(Self::new(pool, logger))
    }

    pub fn get_conn(&self) -> Result<Conn, Error> {
        Ok(self.pool.get()?)
    }

    pub fn sync_block(
        &self,
        block: &Block,
        block_contents: &BlockContents,
        ledger_db: &LedgerDB,
    ) -> Result<
        (
            crate::db::block_audit_data::BlockAuditData,
            HashMap<TokenId, u64>,
        ),
        Error,
    > {
        let conn = self.get_conn()?;
        self.sync_block_with_conn(&conn, block, block_contents, ledger_db)
    }

    pub fn sync_block_with_conn(
        &self,
        conn: &Conn,
        block: &Block,
        block_contents: &BlockContents,
        ledger_db: &LedgerDB,
    ) -> Result<
        (
            crate::db::block_audit_data::BlockAuditData,
            HashMap<TokenId, u64>,
        ),
        Error,
    > {
        transaction(&conn, |conn| {
            let block_index = block.index;
            log::info!(self.logger, "Syncing block {}", block_index);

            let mut counters = Counters::get(conn)?;

            // Ensure that we are syncing the next block and haven't skipped any blocks (or
            // went backwards).
            let next_block_index = counters.num_blocks_synced as BlockIndex;
            if block_index != next_block_index {
                return Err(Error::UnexpectedBlockIndex(block_index, next_block_index));
            }

            // Get balance map for the previous block
            let mut balance_map = if block_index == 0 {
                Default::default()
            } else {
                BlockBalance::get_balances_for_block(&conn, block_index - 1)?
            };

            // Count mints.
            for mint_tx in &block_contents.mint_txs {
                let balance = balance_map
                    .entry(TokenId::from(mint_tx.prefix.token_id))
                    .or_default();

                *balance += mint_tx.prefix.amount;
                log::info!(
                    self.logger,
                    "Block {}: Minted {} of token id {}, balance is now {}",
                    block_index,
                    mint_tx.prefix.amount,
                    mint_tx.prefix.token_id,
                    balance,
                );

                // See if this mint matches an active mint configuration.
                match ledger_db.get_active_mint_config_for_mint_tx(mint_tx) {
                    Ok(_active_mint_config) => {
                        // Got a match, which is what we were hoping would
                        // happen.
                    }
                    Err(err @ LedgerDbError::NotFound)
                    | Err(err @ LedgerDbError::MintLimitExceeded(_, _, _)) => {
                        log::crit!(
                            self.logger,
                            "Block {}: Found mint tx {} that did not match any active mint config: {}",
                            block_index,
                            mint_tx,
                            err,
                        );

                        counters.num_mint_txs_without_matching_mint_config += 1;
                    }
                    Err(err) => {
                        return Err(err.into());
                    }
                }
            }

            // Count burns.
            for tx_out in &block_contents.outputs {
                if let Ok((amount, _)) = tx_out.view_key_match(&burn_address_view_private()) {
                    let balance = balance_map.entry(amount.token_id).or_default();

                    if amount.value > *balance {
                        log::crit!(
                            self.logger,
                            "Block {}: Burned {} of token id {} but only had {}. Setting balance to 0",
                            block_index,
                            amount.value,
                            amount.token_id,
                            balance
                        );
                        *balance = 0;
                        counters.num_burns_exceeding_balance += 1;
                    } else {
                        *balance -= amount.value;
                        log::info!(
                            self.logger,
                            "Block {}: Burned {} of token id {}, balance is now {}",
                            block_index,
                            amount.value,
                            amount.token_id,
                            balance,
                        );
                    }
                }
            }

            // Update the database.
            counters.num_blocks_synced += 1;
            counters.set(conn)?;

            let block_audit_data = BlockAuditData {
                block_index: block_index as i64,
            };
            block_audit_data.set(conn)?;

            BlockBalance::set_balances_for_block(&conn, block_index, &balance_map)?;

            // Success.
            Ok((block_audit_data, balance_map))
        })
    }

    // /// Get the last synced block index, or None if no blocks were synced.
    // pub fn last_synced_block_index(&self) -> Result<Option<u64>, Error> {
    //     match self.get_counters()?.num_blocks_synced {
    //         0 => Ok(None),
    //         val => Ok(Some(val - 1)),
    //     }
    // }

    // /// Get the audit data for a given block index.
    // pub fn get_block_audit_data(&self, block_index: BlockIndex) ->
    // Result<BlockAuditData, Error> {     let db_txn =
    // self.env.begin_ro_txn()?;     self.get_block_audit_data_impl(block_index,
    // &db_txn) }

    // /// Sync mint data from a given block.
    // pub fn sync_block(
    //     &self,
    //     block: &Block,
    //     block_contents: &BlockContents,
    // ) -> Result<BlockAuditData, Error> {

    // pub fn run_migrations(conn: &SqliteConnection) {
    //     // Our migrations sometimes violate foreign keys, so disable foreign key
    // checks     // while we apply them.
    //     // This has to happen outside the scope of a transaction. Quoting
    //     // https://www.sqlite.org/pragma.html,
    //     // "This pragma is a no-op within a transaction; foreign key constraint
    //     // enforcement may only be enabled or disabled when there is no pending
    //     // BEGIN or SAVEPOINT."
    //     // Check foreign key constraints after the migration. If they fail,
    //     // we will abort until the user resolves it.
    //     conn.batch_execute("PRAGMA foreign_keys = OFF;")
    //         .expect("failed disabling foreign keys");
    //     embedded_migrations::run_with_output(conn, &mut std::io::stdout())
    //         .expect("failed running migrations");
    //     WalletDb::validate_foreign_keys(conn);
    //     conn.batch_execute("PRAGMA foreign_keys = ON;")
    //         .expect("failed enabling foreign keys");
    // }
}

/// Create a SQLite transaction with retry.
pub fn transaction<T, E, F>(conn: &Conn, f: F) -> Result<T, E>
where
    F: Clone + FnOnce(&Conn) -> Result<T, E>,
    E: From<diesel::result::Error>,
{
    for i in 0..NUM_RETRIES {
        let f = f.clone();
        let r = conn.transaction::<T, E, _>(|| f(conn));
        // TODO need to only retry in certain conditions!
        if r.is_ok() || i == (NUM_RETRIES - 1) {
            return r;
        }
        sleep(Duration::from_millis((BASE_DELAY_MS * 2_u32.pow(i)) as u64));
    }
    panic!("Should never reach this point.");
}
const BASE_DELAY_MS: u32 = 10;
const NUM_RETRIES: u32 = 5;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_utils::TestDbContext;
    use mc_account_keys::{burn_address, AccountKey};
    use mc_common::logger::{test_with_logger, Logger};
    use mc_crypto_keys::RistrettoPrivate;
    use mc_ledger_db::Ledger;
    use mc_transaction_core::{tx::TxOut, Amount, BlockVersion, TokenId};
    use mc_transaction_core_test_utils::{
        create_ledger, create_mint_config_tx_and_signers, create_mint_tx, create_test_tx_out,
        initialize_ledger, mint_config_tx_to_validated as to_validated, KeyImage,
    };
    use mc_util_from_random::FromRandom;
    use rand_core::SeedableRng;
    use rand_hc::Hc128Rng;
    use std::iter::FromIterator;

    #[test_with_logger]
    fn test_sync_block_happy_flow(logger: Logger) {
        let mut rng = Hc128Rng::from_seed([1u8; 32]);
        let token_id1 = TokenId::from(1);
        let token_id2 = TokenId::from(22);
        let token_id3 = TokenId::from(3);

        let test_db_context = TestDbContext::default();
        let mint_audit_db = test_db_context.get_db_instance(logger.clone());
        let conn = mint_audit_db.get_conn().unwrap();

        let mut ledger_db = create_ledger();
        let account_key = AccountKey::random(&mut rng);
        let initial_num_blocks = 3;
        initialize_ledger(
            BlockVersion::MAX,
            &mut ledger_db,
            initial_num_blocks,
            &account_key,
            &mut rng,
        );

        // The blocks we currently have in the ledger contain no burning or minting.
        for block_index in 0..initial_num_blocks {
            let block_data = ledger_db.get_block_data(block_index).unwrap();

            let (mint_audit_data, balance_map) = mint_audit_db
                .sync_block(block_data.block(), block_data.contents(), &ledger_db)
                .unwrap();

            assert_eq!(
                mint_audit_data,
                BlockAuditData {
                    block_index: block_data.block().index as i64,
                }
            );
            assert_eq!(balance_map, Default::default());
        }

        // Sync a block that contains MintConfigTxs so that we have valid
        // active configs.
        let (mint_config_tx1, signers1) = create_mint_config_tx_and_signers(token_id1, &mut rng);
        let (mint_config_tx2, signers2) = create_mint_config_tx_and_signers(token_id2, &mut rng);
        let (mint_config_tx3, signers3) = create_mint_config_tx_and_signers(token_id3, &mut rng);

        let block_contents = BlockContents {
            validated_mint_config_txs: vec![
                to_validated(&mint_config_tx1),
                to_validated(&mint_config_tx2),
                to_validated(&mint_config_tx3),
            ],
            ..Default::default()
        };

        let parent_block = ledger_db
            .get_block(ledger_db.num_blocks().unwrap() - 1)
            .unwrap();
        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &parent_block,
            &Default::default(),
            &block_contents,
        );

        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();

        let (mint_audit_data, balance_map) = mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();
        assert_eq!(
            mint_audit_data,
            BlockAuditData {
                block_index: block.index as i64,
            }
        );
        assert_eq!(balance_map, Default::default());

        // Sync a block that contains a few mint transactions.
        let mint_tx1 = create_mint_tx(token_id1, &signers1, 1, &mut rng);
        let mint_tx2 = create_mint_tx(token_id2, &signers2, 2, &mut rng);
        let mint_tx3 = create_mint_tx(token_id1, &signers1, 100, &mut rng);

        let block_contents = BlockContents {
            mint_txs: vec![mint_tx1, mint_tx2, mint_tx3],
            outputs: (0..3).map(|_i| create_test_tx_out(&mut rng)).collect(),
            ..Default::default()
        };

        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &block,
            &Default::default(),
            &block_contents,
        );

        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();

        let (mint_audit_data, balance_map) = mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();
        assert_eq!(
            mint_audit_data,
            BlockAuditData {
                block_index: block.index as i64,
            }
        );
        assert_eq!(
            balance_map,
            HashMap::from_iter([(token_id1, 101), (token_id2, 2)])
        );

        // Sync a block with two burn transactions and some unrelated
        // transaction.
        let burn_recipient = burn_address();

        let tx_out1 = TxOut::new(
            Amount {
                value: 50,
                token_id: token_id1,
            },
            &burn_recipient,
            &RistrettoPrivate::from_random(&mut rng),
            Default::default(),
        )
        .unwrap();

        let tx_out2 = TxOut::new(
            Amount {
                value: 10,
                token_id: token_id1,
            },
            &burn_recipient,
            &RistrettoPrivate::from_random(&mut rng),
            Default::default(),
        )
        .unwrap();

        let tx_out3 = create_test_tx_out(&mut rng);

        let block_contents = BlockContents {
            outputs: vec![tx_out1, tx_out2, tx_out3],
            key_images: vec![KeyImage::from(1)],
            ..Default::default()
        };

        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &block,
            &Default::default(),
            &block_contents,
        );

        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();

        let (mint_audit_data, balance_map) = mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();
        assert_eq!(
            mint_audit_data,
            BlockAuditData {
                block_index: block.index as i64,
            }
        );

        assert_eq!(
            balance_map,
            HashMap::from_iter([(token_id1, 41), (token_id2, 2)]),
        );

        // Sync a block that mixes burning and minting.
        let mint_tx1 = create_mint_tx(token_id1, &signers1, 1000, &mut rng);
        let mint_tx2 = create_mint_tx(token_id2, &signers2, 2000, &mut rng);
        let mint_tx3 = create_mint_tx(token_id3, &signers3, 20000, &mut rng);

        let tx_out1 = TxOut::new(
            Amount {
                value: 900,
                token_id: token_id1,
            },
            &burn_recipient,
            &RistrettoPrivate::from_random(&mut rng),
            Default::default(),
        )
        .unwrap();

        let tx_out2 = TxOut::new(
            Amount {
                value: 1000,
                token_id: token_id2,
            },
            &burn_recipient,
            &RistrettoPrivate::from_random(&mut rng),
            Default::default(),
        )
        .unwrap();

        let tx_out3 = create_test_tx_out(&mut rng);

        let block_contents = BlockContents {
            outputs: vec![tx_out1, tx_out2, tx_out3],
            mint_txs: vec![mint_tx1, mint_tx2, mint_tx3],
            ..Default::default()
        };

        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &block,
            &Default::default(),
            &block_contents,
        );
        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();

        let (mint_audit_data, balance_map) = mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();
        assert_eq!(
            mint_audit_data,
            BlockAuditData {
                block_index: block.index as i64,
            }
        );

        assert_eq!(
            balance_map,
            HashMap::from_iter([(token_id1, 141), (token_id2, 1002), (token_id3, 20000)]),
        );

        // Sanity check counters.
        assert_eq!(
            Counters::get(&conn).unwrap(),
            Counters {
                id: 0,
                num_blocks_synced: block.index as i64 + 1,
                num_burns_exceeding_balance: 0,
                num_mint_txs_without_matching_mint_config: 0,
            }
        );
    }

    // Attempting to skip a block when syncing should fail.
    #[test_with_logger]
    fn test_sync_block_refuses_skipping_a_block(logger: Logger) {
        let mut rng = Hc128Rng::from_seed([1u8; 32]);

        let test_db_context = TestDbContext::default();
        let mint_audit_db = test_db_context.get_db_instance(logger.clone());
        let mut ledger_db = create_ledger();
        let account_key = AccountKey::random(&mut rng);
        let initial_num_blocks = 3;
        initialize_ledger(
            BlockVersion::MAX,
            &mut ledger_db,
            initial_num_blocks,
            &account_key,
            &mut rng,
        );

        // Sync the first block, this should succeed.
        let block_data = ledger_db.get_block_data(0).unwrap();
        mint_audit_db
            .sync_block(block_data.block(), block_data.contents(), &ledger_db)
            .unwrap();

        // Syncing the third block should fail since we haven't synced the second block.
        let block_data = ledger_db.get_block_data(2).unwrap();
        assert!(matches!(
            mint_audit_db.sync_block(block_data.block(), block_data.contents(), &ledger_db),
            Err(Error::UnexpectedBlockIndex(2, 1))
        ));
    }

    // Attempting to sync the same block twice should fail.
    #[test_with_logger]
    fn test_sync_block_refuses_same_block(logger: Logger) {
        let mut rng = Hc128Rng::from_seed([1u8; 32]);

        let test_db_context = TestDbContext::default();
        let mint_audit_db = test_db_context.get_db_instance(logger.clone());
        let mut ledger_db = create_ledger();
        let account_key = AccountKey::random(&mut rng);
        let initial_num_blocks = 3;
        initialize_ledger(
            BlockVersion::MAX,
            &mut ledger_db,
            initial_num_blocks,
            &account_key,
            &mut rng,
        );

        // Sync the first block, this should succeed.
        let block_data = ledger_db.get_block_data(0).unwrap();
        mint_audit_db
            .sync_block(block_data.block(), block_data.contents(), &ledger_db)
            .unwrap();

        // Syncing it again should fail.
        assert!(matches!(
            mint_audit_db.sync_block(block_data.block(), block_data.contents(), &ledger_db),
            Err(Error::UnexpectedBlockIndex(0, 1))
        ));
    }

    // Attempting to sync an old block should fail.
    #[test_with_logger]
    fn test_sync_block_refuses_going_backwards(logger: Logger) {
        let mut rng = Hc128Rng::from_seed([1u8; 32]);

        let test_db_context = TestDbContext::default();
        let mint_audit_db = test_db_context.get_db_instance(logger.clone());

        let mut ledger_db = create_ledger();
        let account_key = AccountKey::random(&mut rng);
        let initial_num_blocks = 3;
        initialize_ledger(
            BlockVersion::MAX,
            &mut ledger_db,
            initial_num_blocks,
            &account_key,
            &mut rng,
        );

        for block_index in 0..initial_num_blocks {
            let block_data = ledger_db.get_block_data(block_index).unwrap();

            mint_audit_db
                .sync_block(block_data.block(), block_data.contents(), &ledger_db)
                .unwrap();
        }
        // Syncing the first block should fail since we already synced it.
        let block_data = ledger_db.get_block_data(0).unwrap();
        assert!(matches!(
            mint_audit_db.sync_block(block_data.block(), block_data.contents(), &ledger_db),
            Err(Error::UnexpectedBlockIndex(0, 3))
        ));
    }

    // Attempting to burn more than the calculated balance result in the counter
    // being increased.
    #[test_with_logger]
    fn test_sync_block_increases_counter_on_over_burn(logger: Logger) {
        let mut rng = Hc128Rng::from_seed([1u8; 32]);
        let token_id1 = TokenId::from(1);
        let token_id2 = TokenId::from(22);

        let test_db_context = TestDbContext::default();
        let mint_audit_db = test_db_context.get_db_instance(logger.clone());
        let conn = mint_audit_db.get_conn().unwrap();

        let mut ledger_db = create_ledger();
        let account_key = AccountKey::random(&mut rng);
        let initial_num_blocks = 3;
        initialize_ledger(
            BlockVersion::MAX,
            &mut ledger_db,
            initial_num_blocks,
            &account_key,
            &mut rng,
        );

        // The blocks we currently have in the ledger contain no burning or minting.
        for block_index in 0..initial_num_blocks {
            let block_data = ledger_db.get_block_data(block_index).unwrap();

            mint_audit_db
                .sync_block(block_data.block(), block_data.contents(), &ledger_db)
                .unwrap();
        }

        // Sync a block that contains a few mint transactions.
        let (mint_config_tx1, signers1) = create_mint_config_tx_and_signers(token_id1, &mut rng);
        let (mint_config_tx2, signers2) = create_mint_config_tx_and_signers(token_id2, &mut rng);

        let block_contents = BlockContents {
            validated_mint_config_txs: vec![
                to_validated(&mint_config_tx1),
                to_validated(&mint_config_tx2),
            ],
            ..Default::default()
        };
        let parent_block = ledger_db
            .get_block(ledger_db.num_blocks().unwrap() - 1)
            .unwrap();
        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &parent_block,
            &Default::default(),
            &block_contents,
        );
        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();
        mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();

        let mint_tx1 = create_mint_tx(token_id1, &signers1, 1, &mut rng);
        let mint_tx2 = create_mint_tx(token_id2, &signers2, 2, &mut rng);
        let mint_tx3 = create_mint_tx(token_id1, &signers1, 100, &mut rng);

        let block_contents = BlockContents {
            mint_txs: vec![mint_tx1, mint_tx2, mint_tx3],
            outputs: (0..3).map(|_i| create_test_tx_out(&mut rng)).collect(),
            ..Default::default()
        };

        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &block,
            &Default::default(),
            &block_contents,
        );

        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();

        let (mint_audit_data, balance_map) = mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();
        assert_eq!(
            mint_audit_data,
            BlockAuditData {
                block_index: block.index as i64,
            }
        );
        assert_eq!(
            balance_map,
            HashMap::from_iter([(token_id1, 101), (token_id2, 2)])
        );

        // At this point nothing has been over-burned.
        assert_eq!(Counters::get(&conn).unwrap().num_burns_exceeding_balance, 0);

        // Sync a block with two burn transactions that results in one of them
        // over-burning.
        let burn_recipient = burn_address();

        let tx_out1 = TxOut::new(
            Amount {
                value: 50000,
                token_id: token_id1,
            },
            &burn_recipient,
            &RistrettoPrivate::from_random(&mut rng),
            Default::default(),
        )
        .unwrap();

        let tx_out2 = TxOut::new(
            Amount {
                value: 2,
                token_id: token_id2,
            },
            &burn_recipient,
            &RistrettoPrivate::from_random(&mut rng),
            Default::default(),
        )
        .unwrap();

        let tx_out3 = create_test_tx_out(&mut rng);

        let block_contents = BlockContents {
            outputs: vec![tx_out1, tx_out2, tx_out3],
            key_images: vec![KeyImage::from(1)],
            ..Default::default()
        };

        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &block,
            &Default::default(),
            &block_contents,
        );

        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();

        let (mint_audit_data, balance_map) = mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();
        assert_eq!(
            mint_audit_data,
            BlockAuditData {
                block_index: block.index as i64
            },
        );
        assert_eq!(
            balance_map,
            HashMap::from_iter([(token_id1, 0), (token_id2, 0)]),
        );

        // Over-burn has been recorded.
        assert_eq!(Counters::get(&conn).unwrap().num_burns_exceeding_balance, 1);

        // Over burn once again, see that counter increases.
        let tx_out1 = TxOut::new(
            Amount {
                value: 50000,
                token_id: token_id1,
            },
            &burn_recipient,
            &RistrettoPrivate::from_random(&mut rng),
            Default::default(),
        )
        .unwrap();

        let tx_out2 = TxOut::new(
            Amount {
                value: 2,
                token_id: token_id2,
            },
            &burn_recipient,
            &RistrettoPrivate::from_random(&mut rng),
            Default::default(),
        )
        .unwrap();

        let tx_out3 = create_test_tx_out(&mut rng);

        let block_contents = BlockContents {
            outputs: vec![tx_out1, tx_out2, tx_out3],
            key_images: vec![KeyImage::from(2)],
            ..Default::default()
        };

        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &block,
            &Default::default(),
            &block_contents,
        );
        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();

        mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();

        assert_eq!(Counters::get(&conn).unwrap().num_burns_exceeding_balance, 3);
    }

    // MintTxs that do not match an active MintConfig get counted.
    #[test_with_logger]
    fn test_sync_block_counts_mint_txs_without_active_config(logger: Logger) {
        let mut rng = Hc128Rng::from_seed([1u8; 32]);
        let token_id1 = TokenId::from(1);
        let token_id2 = TokenId::from(22);

        let test_db_context = TestDbContext::default();
        let mint_audit_db = test_db_context.get_db_instance(logger.clone());
        let conn = mint_audit_db.get_conn().unwrap();

        let mut ledger_db = create_ledger();
        let account_key = AccountKey::random(&mut rng);
        let initial_num_blocks = 3;
        initialize_ledger(
            BlockVersion::MAX,
            &mut ledger_db,
            initial_num_blocks,
            &account_key,
            &mut rng,
        );

        // The blocks we currently have in the ledger contain no burning or minting.
        for block_index in 0..initial_num_blocks {
            let block_data = ledger_db.get_block_data(block_index).unwrap();

            mint_audit_db
                .sync_block(block_data.block(), block_data.contents(), &ledger_db)
                .unwrap();
        }

        // Sync a block that contains MintConfigTxs so that we have valid active
        // configs.
        let (mint_config_tx1, signers1) = create_mint_config_tx_and_signers(token_id1, &mut rng);
        let (mint_config_tx2, signers2) = create_mint_config_tx_and_signers(token_id2, &mut rng);

        let block_contents = BlockContents {
            validated_mint_config_txs: vec![
                to_validated(&mint_config_tx1),
                to_validated(&mint_config_tx2),
            ],
            ..Default::default()
        };

        let parent_block = ledger_db
            .get_block(ledger_db.num_blocks().unwrap() - 1)
            .unwrap();
        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &parent_block,
            &Default::default(),
            &block_contents,
        );

        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();
        mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();

        // Sync a block that contains a mint transaction with incorrect signers.
        // Normally we would append the block to the ledger and test as usual, but since
        // it contains an invalid MintTx append_block would actually fail. As
        // such we do this inside a transaction and then roll back. TODO explain
        // more
        {
            let mint_tx1 = create_mint_tx(token_id1, &signers2, 1, &mut rng);

            let block_contents = BlockContents {
                mint_txs: vec![mint_tx1],
                outputs: (0..3).map(|_i| create_test_tx_out(&mut rng)).collect(),
                ..Default::default()
            };

            let block = Block::new_with_parent(
                BlockVersion::MAX,
                &block,
                &Default::default(),
                &block_contents,
            );

            let _ = transaction(&conn, |conn| -> Result<(), Error> {
                mint_audit_db
                    .sync_block_with_conn(conn, &block, &block_contents, &ledger_db)
                    .unwrap();

                assert_eq!(
                    Counters::get(conn).unwrap(),
                    Counters {
                        id: 0,
                        num_blocks_synced: block.index as i64 + 1,
                        num_burns_exceeding_balance: 0,
                        num_mint_txs_without_matching_mint_config: 1,
                    }
                );

                return Err(Error::NotFound); // Chose arbitrarily, we just need
                                             // to
                                             // return an error to ensure the
                                             // transaction gets rolled back.
            });
        }

        // Sync a block that invalidates the previous configs.
        let (mint_config_tx3, signers3) = create_mint_config_tx_and_signers(token_id1, &mut rng);

        let block_contents = BlockContents {
            validated_mint_config_txs: vec![to_validated(&mint_config_tx3)],
            ..Default::default()
        };

        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &block,
            &Default::default(),
            &block_contents,
        );

        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();
        mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();

        // Sync a block that contains a mint transaction with signers that refer to a no
        // longer valid mint config.
        {
            let mint_tx2 = create_mint_tx(token_id1, &signers1, 1, &mut rng);

            let block_contents = BlockContents {
                mint_txs: vec![mint_tx2],
                outputs: (0..3).map(|_i| create_test_tx_out(&mut rng)).collect(),
                ..Default::default()
            };

            let block = Block::new_with_parent(
                BlockVersion::MAX,
                &block,
                &Default::default(),
                &block_contents,
            );

            let _ = transaction(&conn, |conn| -> Result<(), Error> {
                mint_audit_db
                    .sync_block_with_conn(conn, &block, &block_contents, &ledger_db)
                    .unwrap();

                assert_eq!(
                    Counters::get(&conn).unwrap(),
                    Counters {
                        id: 0,
                        num_blocks_synced: block.index as i64 + 1,
                        num_burns_exceeding_balance: 0,
                        num_mint_txs_without_matching_mint_config: 1,
                    }
                );
                return Err(Error::NotFound); // Chose arbitrarily, we just need
                                             // to
                                             // return an error to ensure the
                                             // transaction gets rolled back.
            });
        }

        // Sanity - sync a block with a MintTx that matches a valid config.
        let mint_tx3 = create_mint_tx(token_id1, &signers3, 1, &mut rng);

        let block_contents = BlockContents {
            mint_txs: vec![mint_tx3],
            outputs: (0..3).map(|_i| create_test_tx_out(&mut rng)).collect(),
            ..Default::default()
        };

        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &block,
            &Default::default(),
            &block_contents,
        );

        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();
        mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();

        assert_eq!(
            Counters::get(&conn).unwrap(),
            Counters {
                id: 0,
                num_blocks_synced: block.index as i64 + 1,
                num_burns_exceeding_balance: 0,
                num_mint_txs_without_matching_mint_config: 0,
            }
        );
    }

    // MintTxs that exceed the MintConfigTx limit get counted.
    #[test_with_logger]
    fn test_sync_blocks_counts_mint_txs_exceeding_total_mint_limit(logger: Logger) {
        let mut rng = Hc128Rng::from_seed([1u8; 32]);
        let token_id1 = TokenId::from(1);

        let test_db_context = TestDbContext::default();
        let mint_audit_db = test_db_context.get_db_instance(logger.clone());
        let conn = mint_audit_db.get_conn().unwrap();

        let mut ledger_db = create_ledger();
        let account_key = AccountKey::random(&mut rng);
        let initial_num_blocks = 3;
        initialize_ledger(
            BlockVersion::MAX,
            &mut ledger_db,
            initial_num_blocks,
            &account_key,
            &mut rng,
        );

        // The blocks we currently have in the ledger contain no burning or minting.
        for block_index in 0..initial_num_blocks {
            let block_data = ledger_db.get_block_data(block_index).unwrap();

            mint_audit_db
                .sync_block(block_data.block(), block_data.contents(), &ledger_db)
                .unwrap();
        }

        // Sync a block that contains a MintConfigTx with a total limit we are able to
        // exceed.
        let (mut mint_config_tx1, signers1) =
            create_mint_config_tx_and_signers(token_id1, &mut rng);
        mint_config_tx1.prefix.total_mint_limit = 2;

        assert!(
            mint_config_tx1.prefix.configs[0].mint_limit > mint_config_tx1.prefix.total_mint_limit
        );

        let block_contents = BlockContents {
            validated_mint_config_txs: vec![to_validated(&mint_config_tx1)],
            ..Default::default()
        };

        let parent_block = ledger_db
            .get_block(ledger_db.num_blocks().unwrap() - 1)
            .unwrap();
        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &parent_block,
            &Default::default(),
            &block_contents,
        );

        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();
        mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();

        // Sync a block that mints the total mint limit.
        let mint_tx1 = create_mint_tx(
            token_id1,
            &signers1,
            mint_config_tx1.prefix.total_mint_limit,
            &mut rng,
        );

        let block_contents = BlockContents {
            mint_txs: vec![mint_tx1],
            outputs: (0..3).map(|_i| create_test_tx_out(&mut rng)).collect(),
            ..Default::default()
        };

        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &block,
            &Default::default(),
            &block_contents,
        );

        mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();
        ledger_db
            .append_block(&block, &block_contents, None)
            .unwrap();

        assert_eq!(
            Counters::get(&conn).unwrap(),
            Counters {
                id: 0,
                num_blocks_synced: block.index as i64 + 1,
                num_burns_exceeding_balance: 0,
                num_mint_txs_without_matching_mint_config: 0,
            }
        );

        // Minting more should get flagged since we are exceeding the total mint limit.
        let mint_tx1 = create_mint_tx(token_id1, &signers1, 1, &mut rng);

        let block_contents = BlockContents {
            mint_txs: vec![mint_tx1],
            outputs: (0..3).map(|_i| create_test_tx_out(&mut rng)).collect(),
            ..Default::default()
        };

        let block = Block::new_with_parent(
            BlockVersion::MAX,
            &block,
            &Default::default(),
            &block_contents,
        );

        mint_audit_db
            .sync_block(&block, &block_contents, &ledger_db)
            .unwrap();

        assert_eq!(
            Counters::get(&conn).unwrap(),
            Counters {
                id: 0,
                num_blocks_synced: block.index as i64 + 1,
                num_burns_exceeding_balance: 0,
                num_mint_txs_without_matching_mint_config: 1,
            }
        );
    }
}
