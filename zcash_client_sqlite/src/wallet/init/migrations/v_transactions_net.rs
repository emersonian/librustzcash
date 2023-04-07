//! Migration that fixes a bug in v_transactions that caused the change to be incorrectly ignored
//! as received value.
use std::collections::HashSet;

use rusqlite::{self, named_params};
use schemer;
use schemer_rusqlite::RusqliteMigration;
use uuid::Uuid;

use super::add_transaction_views;
use crate::wallet::{init::WalletMigrationError, pool_code, PoolType};

pub(super) const MIGRATION_ID: Uuid = Uuid::from_fields(
    0x2aa4d24f,
    0x51aa,
    0x4a4c,
    b"\x8d\x9b\xe5\xb8\xa7\x62\x86\x5f",
);

pub(crate) struct Migration;

impl schemer::Migration for Migration {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        [add_transaction_views::MIGRATION_ID].into_iter().collect()
    }

    fn description(&self) -> &'static str {
        "Fix transaction views to correctly handle double-entry accounting for change."
    }
}

impl RusqliteMigration for Migration {
    type Error = WalletMigrationError;

    fn up(&self, transaction: &rusqlite::Transaction) -> Result<(), WalletMigrationError> {
        // As of this migration, the `received_notes` table only stores Sapling notes, so
        // we can hard-code the `output_pool` value.
        transaction.execute(
            "INSERT INTO sent_notes (tx, output_pool, output_index, from_account, to_account, value)
             SELECT tx, :output_pool, output_index, account, account, value
             FROM received_notes
             WHERE received_notes.is_change
             EXCEPT
             SELECT tx, :output_pool, output_index, from_account, from_account, value
             FROM sent_notes",
             named_params![
                ":output_pool": &pool_code(PoolType::Sapling)
             ]
        )?;

        transaction.execute_batch(
            "DROP VIEW v_tx_received;
             DROP VIEW v_tx_sent;",
        )?;

        transaction.execute_batch(
            "CREATE VIEW v_tx_events AS
            SELECT received_notes.tx           AS id_tx,
                   received_notes.output_index AS output_index,
                   sent_notes.from_account     AS from_account,
                   received_notes.account      AS to_account,
                   NULL                        AS to_address,
                   received_notes.value        AS value,
                   received_notes.is_change    AS is_change,
                   received_notes.memo         AS memo
            FROM received_notes
            LEFT JOIN sent_notes
                      ON sent_notes.tx = received_notes.tx
                      AND sent_notes.output_index = received_notes.output_index
            UNION 
            SELECT sent_notes.tx               AS id_tx,
                   sent_notes.output_index     AS output_index,
                   sent_notes.from_account     AS from_account,
                   received_notes.account      AS to_account,
                   sent_notes.to_address       AS to_address,
                   sent_notes.value            AS value,
                   false                       AS is_change,
                   sent_notes.memo             AS memo
            FROM sent_notes
            LEFT JOIN received_notes
                      ON received_notes.tx = sent_notes.tx
                      AND received_notes.output_index = sent_notes.output_index
            WHERE  received_notes.is_change IS NULL
               OR  received_notes.is_change = 0",
        )?;

        transaction.execute_batch(
            "DROP VIEW v_transactions;
            CREATE VIEW v_transactions AS
            WITH
            notes AS (
                SELECT received_notes.account        AS account_id,
                       received_notes.tx             AS id_tx,
                       received_notes.value          AS value,
                       CASE
                            WHEN received_notes.is_change THEN 1
                            ELSE 0
                       END AS is_change,
                       CASE
                            WHEN received_notes.is_change THEN 0
                            ELSE 1
                       END AS received_count,
                       CASE
                           WHEN received_notes.memo IS NULL THEN 0
                           ELSE 1
                       END AS memo_present
                FROM   received_notes
                UNION
                SELECT received_notes.account        AS account_id,
                       received_notes.spent          AS id_tx,
                       -received_notes.value         AS value,
                       0                             AS is_change,
                       0                             AS received_count,
                       0                             AS memo_present
                FROM   received_notes
                WHERE  received_notes.spent IS NOT NULL
            ),
            sent_note_counts AS (
                SELECT from_account AS account_id,
                       tx AS id_tx,
                       COUNT(DISTINCT id_note) as sent_notes,
                       SUM(
                         CASE
                             WHEN sent_notes.memo IS NULL THEN 0
                             ELSE 1
                         END
                       ) AS memo_count
                FROM sent_notes
                WHERE (sent_notes.tx, sent_notes.output_index) NOT IN (
                    SELECT received_notes.tx, received_notes.output_index FROM received_notes
                    WHERE received_notes.is_change = 1
                )
                GROUP BY account_id, id_tx
            ),
            blocks_max_height AS (
                SELECT MAX(blocks.height) as max_height FROM blocks
            )
            SELECT notes.account_id                  AS account_id,
                   transactions.id_tx                AS id_tx,
                   transactions.block                AS mined_height,
                   transactions.tx_index             AS tx_index,
                   transactions.txid                 AS txid,
                   transactions.expiry_height        AS expiry_height,
                   transactions.raw                  AS raw,
                   SUM(notes.value)                  AS net_transfer,
                   transactions.fee                  AS fee_paid,
                   SUM(notes.is_change) > 0          AS has_change,
                   MAX(COALESCE(sent_note_counts.sent_notes, 0))  AS sent_note_count,
                   SUM(notes.received_count)         AS received_note_count,
                   SUM(notes.memo_present) + MAX(COALESCE(sent_note_counts.memo_count, 0)) AS memo_count,
                   blocks.time                       AS block_time,
                   (
                        blocks.height IS NULL 
                        AND transactions.expiry_height <= blocks_max_height.max_height
                    ) AS expired_unmined
            FROM transactions
            JOIN notes ON notes.id_tx = transactions.id_tx
            JOIN blocks_max_height
            LEFT JOIN blocks ON blocks.height = transactions.block
            LEFT JOIN sent_note_counts
                      ON sent_note_counts.account_id = notes.account_id
                      AND sent_note_counts.id_tx = notes.id_tx
            GROUP BY notes.account_id, transactions.id_tx",
        )?;

        Ok(())
    }

    fn down(&self, _transaction: &rusqlite::Transaction) -> Result<(), WalletMigrationError> {
        // TODO: something better than just panic?
        panic!("Cannot revert this migration.");
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::{self, params};
    use tempfile::NamedTempFile;

    use zcash_client_backend::keys::UnifiedSpendingKey;
    use zcash_primitives::zip32::AccountId;

    use crate::{
        tests,
        wallet::init::{init_wallet_db_internal, migrations::add_transaction_views},
        WalletDb,
    };

    #[test]
    fn v_transactions_net() {
        let data_file = NamedTempFile::new().unwrap();
        let mut db_data = WalletDb::for_path(data_file.path(), tests::network()).unwrap();
        init_wallet_db_internal(&mut db_data, None, &[add_transaction_views::MIGRATION_ID])
            .unwrap();

        // Create two accounts in the wallet.
        let usk0 =
            UnifiedSpendingKey::from_seed(&tests::network(), &[0u8; 32][..], AccountId::from(0))
                .unwrap();
        let ufvk0 = usk0.to_unified_full_viewing_key();
        db_data
            .conn
            .execute(
                "INSERT INTO accounts (account, ufvk) VALUES (0, ?)",
                params![ufvk0.encode(&tests::network())],
            )
            .unwrap();

        let usk1 =
            UnifiedSpendingKey::from_seed(&tests::network(), &[1u8; 32][..], AccountId::from(1))
                .unwrap();
        let ufvk1 = usk1.to_unified_full_viewing_key();
        db_data
            .conn
            .execute(
                "INSERT INTO accounts (account, ufvk) VALUES (1, ?)",
                params![ufvk1.encode(&tests::network())],
            )
            .unwrap();

        // - Tx 0 contains two received notes of 2 and 5 zatoshis that are controlled by account 0.
        db_data.conn.execute_batch(
            "INSERT INTO blocks (height, hash, time, sapling_tree) VALUES (0, 0, 0, '');
            INSERT INTO transactions (block, id_tx, txid) VALUES (0, 0, 'tx0');

            INSERT INTO received_notes (tx, output_index, account, diversifier, value, rcm, nf, is_change)
            VALUES (0, 0, 0, '', 2, '', 'nf_a', false);
            INSERT INTO received_notes (tx, output_index, account, diversifier, value, rcm, nf, is_change)
            VALUES (0, 3, 0, '', 5, '', 'nf_b', false);").unwrap();

        // - Tx 1 creates two notes of 2 and 3 zatoshis for an external address, and a change note
        //   of 2 zatoshis. This is representative of a historic transaction where no `sent_notes`
        //   entry was created for the change value.
        db_data.conn.execute_batch(
            "INSERT INTO blocks (height, hash, time, sapling_tree) VALUES (1, 1, 1, '');
            INSERT INTO transactions (block, id_tx, txid) VALUES (1, 1, 'tx1');
            UPDATE received_notes SET spent = 1 WHERE tx = 0;
            INSERT INTO sent_notes (tx, output_pool, output_index, from_account, to_account, to_address, value)
            VALUES (1, 2, 0, 0, NULL, 'addra', 2);
            INSERT INTO sent_notes (tx, output_pool, output_index, from_account, to_account, to_address, value, memo)
            VALUES (1, 2, 1, 0, NULL, 'addrb', 3, X'61');
            INSERT INTO received_notes (tx, output_index, account, diversifier, value, rcm, nf, is_change)
            VALUES (1,    2, 0, '', 2, '', 'nf_c', true);").unwrap();

        // - Tx 2 sends the half of the wallet value from account 0 to account 1 and returns the
        //   other half to the sending account as change.
        db_data.conn.execute_batch(
            "INSERT INTO blocks (height, hash, time, sapling_tree) VALUES (2, 2, 2, '');
            INSERT INTO transactions (block, id_tx, txid) VALUES (2, 2, 'tx2');
            UPDATE received_notes SET spent = 2 WHERE tx = 1;
            INSERT INTO sent_notes (tx, output_pool, output_index, from_account, to_account, to_address, value)
            VALUES (2, 2, 0, 0, 0, NULL, 1);
            INSERT INTO sent_notes (tx, output_pool, output_index, from_account, to_account, to_address, value)
            VALUES (2, 2, 1, 0, 1, NULL, 1);
            INSERT INTO received_notes (tx, output_index, account, diversifier, value, rcm, nf, is_change)
            VALUES (2, 0, 0, '', 1, '', 'nf_d', true);
            INSERT INTO received_notes (tx, output_index, account, diversifier, value, rcm, nf, is_change)
            VALUES (2, 1, 1, '', 1, '', 'nf_e', false);",
        ).unwrap();

        // Behavior prior to change:
        {
            let mut q = db_data
                .conn
                .prepare(
                    "SELECT id_tx, received_by_account, received_total, received_note_count, memo_count
                     FROM v_tx_received",
                )
                .unwrap();
            let mut rows = q.query([]).unwrap();
            let mut row_count = 0;
            while let Some(row) = rows.next().unwrap() {
                row_count += 1;
                let tx: i64 = row.get(0).unwrap();
                let account: i64 = row.get(1).unwrap();
                let total: i64 = row.get(2).unwrap();
                let count: i64 = row.get(3).unwrap();
                let memo_count: i64 = row.get(4).unwrap();
                match (account, tx) {
                    (0, 0) => {
                        assert_eq!(total, 7);
                        assert_eq!(count, 2);
                        assert_eq!(memo_count, 0);
                    }
                    (0, 1) => {
                        // ERROR: transaction 1 only has change, should not be counted as received
                        assert_eq!(total, 2);
                        assert_eq!(count, 1);
                        assert_eq!(memo_count, 0);
                    }
                    (0, 2) => {
                        // ERROR: transaction 2 was counted twice: as a received transfer, and as change
                        assert_eq!(total, 1);
                        assert_eq!(count, 1);
                        assert_eq!(memo_count, 0);
                    }
                    (1, 2) => {
                        // Transaction 2 should be counted as received, as this is a cross-account
                        // transfer, not change.
                        assert_eq!(total, 1);
                        assert_eq!(count, 1);
                        assert_eq!(memo_count, 0);
                    }
                    _ => {
                        panic!("No such transaction.");
                    }
                }
            }
            assert_eq!(row_count, 4);

            let mut q = db_data
                .conn
                .prepare("SELECT id_tx, sent_total, sent_note_count, memo_count FROM v_tx_sent")
                .unwrap();
            let mut rows = q.query([]).unwrap();
            let mut row_count = 0;
            while let Some(row) = rows.next().unwrap() {
                row_count += 1;
                let tx: i64 = row.get(0).unwrap();
                let total: i64 = row.get(1).unwrap();
                let count: i64 = row.get(2).unwrap();
                let memo_count: i64 = row.get(3).unwrap();
                match tx {
                    1 => {
                        assert_eq!(total, 5);
                        assert_eq!(count, 2);
                        assert_eq!(memo_count, 1);
                    }
                    2 => {
                        // ERROR: the total "sent" includes the change
                        assert_eq!(total, 2);
                        assert_eq!(count, 2);
                        assert_eq!(memo_count, 0);
                    }
                    other => {
                        panic!("Transaction {} is not a sent tx.", other);
                    }
                }
            }
            assert_eq!(row_count, 2);
        }

        // Run this migration
        init_wallet_db_internal(&mut db_data, None, &[super::MIGRATION_ID]).unwrap();

        // Corrected behavior after v_transactions has been updated
        {
            let mut q = db_data
                .conn
                .prepare(
                    "SELECT account_id, id_tx, net_transfer, has_change, memo_count, sent_note_count, received_note_count
                    FROM v_transactions",
                )
                .unwrap();
            let mut rows = q.query([]).unwrap();
            let mut row_count = 0;
            while let Some(row) = rows.next().unwrap() {
                row_count += 1;
                let account: i64 = row.get(0).unwrap();
                let tx: i64 = row.get(1).unwrap();
                let net_transfer: i64 = row.get(2).unwrap();
                let has_change: bool = row.get(3).unwrap();
                let memo_count: i64 = row.get(4).unwrap();
                let sent_note_count: i64 = row.get(5).unwrap();
                let received_note_count: i64 = row.get(6).unwrap();
                match (account, tx) {
                    (0, 0) => {
                        assert_eq!(net_transfer, 7);
                        assert!(!has_change);
                        assert_eq!(memo_count, 0);
                    }
                    (0, 1) => {
                        assert_eq!(net_transfer, -5);
                        assert!(has_change);
                        assert_eq!(memo_count, 1);
                    }
                    (0, 2) => {
                        assert_eq!(net_transfer, -1);
                        assert!(has_change);
                        assert_eq!(memo_count, 0);
                        assert_eq!(sent_note_count, 1);
                        assert_eq!(received_note_count, 0);
                    }
                    (1, 2) => {
                        assert_eq!(net_transfer, 1);
                        assert!(!has_change);
                        assert_eq!(memo_count, 0);
                        assert_eq!(sent_note_count, 0);
                        assert_eq!(received_note_count, 1);
                    }
                    other => {
                        panic!("(Account, Transaction) pair {:?} is not expected to exist in the wallet.", other);
                    }
                }
            }
            assert_eq!(row_count, 4);
        }

        // tests for v_tx_events
        {
            let mut q = db_data
                .conn
                .prepare("SELECT * FROM v_tx_events WHERE id_tx = 1")
                .unwrap();
            let mut rows = q.query([]).unwrap();
            let mut row_count = 0;
            while let Some(row) = rows.next().unwrap() {
                row_count += 1;
                let tx: i64 = row.get(0).unwrap();
                let output_index: i64 = row.get(1).unwrap();
                let from_account: Option<i64> = row.get(2).unwrap();
                let to_account: Option<i64> = row.get(3).unwrap();
                let to_address: Option<String> = row.get(4).unwrap();
                let value: i64 = row.get(5).unwrap();
                let is_change: bool = row.get(6).unwrap();
                //let memo: Option<String> = row.get(7).unwrap();
                match output_index {
                    0 => {
                        assert_eq!(from_account, Some(0));
                        assert_eq!(to_account, None);
                        assert_eq!(to_address, Some("addra".to_string()));
                        assert_eq!(value, 2);
                        assert!(!is_change);
                    }
                    1 => {
                        assert_eq!(from_account, Some(0));
                        assert_eq!(to_account, None);
                        assert_eq!(to_address, Some("addrb".to_string()));
                        assert_eq!(value, 3);
                        assert!(!is_change);
                    }
                    2 => {
                        assert_eq!(from_account, Some(0));
                        assert_eq!(to_account, Some(0));
                        assert_eq!(to_address, None);
                        assert_eq!(value, 2);
                        assert!(is_change);
                    }
                    other => {
                        panic!("Unexpected output index for tx {}: {}.", tx, other);
                    }
                }
            }
            assert_eq!(row_count, 3);

            let mut q = db_data
                .conn
                .prepare("SELECT * FROM v_tx_events WHERE id_tx = 2")
                .unwrap();
            let mut rows = q.query([]).unwrap();
            let mut row_count = 0;
            while let Some(row) = rows.next().unwrap() {
                row_count += 1;
                let tx: i64 = row.get(0).unwrap();
                let output_index: i64 = row.get(1).unwrap();
                let from_account: Option<i64> = row.get(2).unwrap();
                let to_account: Option<i64> = row.get(3).unwrap();
                let to_address: Option<String> = row.get(4).unwrap();
                let value: i64 = row.get(5).unwrap();
                let is_change: bool = row.get(6).unwrap();
                match output_index {
                    0 => {
                        assert_eq!(from_account, Some(0));
                        assert_eq!(to_account, Some(0));
                        assert_eq!(to_address, None);
                        assert_eq!(value, 1);
                        assert!(is_change);
                    }
                    1 => {
                        assert_eq!(from_account, Some(0));
                        assert_eq!(to_account, Some(1));
                        assert_eq!(to_address, None);
                        assert_eq!(value, 1);
                        assert!(!is_change);
                    }
                    other => {
                        panic!("Unexpected output index for tx {}: {}.", tx, other);
                    }
                }
            }
            assert_eq!(row_count, 2);
        }
    }
}
