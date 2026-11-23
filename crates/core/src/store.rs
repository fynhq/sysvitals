//! SQLite history storage: single source of truth for CPU/mem; network is never persisted.

use std::cell::Cell;
use std::time::Duration;

use rusqlite::{params, Connection, ErrorCode, TransactionBehavior};

// Raw 2 s samples cover 24 h; 1-min rollups cover retention; wider ranges read rollups, never raw.
pub(crate) const RAW_RETENTION_SECS: i64 = 24 * 60 * 60;

const ROLLUP_MIN_BUCKET_SECS: i64 = 60;

// Schema v2 = raw + 1-min rollup; a failed backfill leaves the store raw-only until a retry succeeds.
const SCHEMA_VERSION: i32 = 2;

// Retention deletes free pages without shrinking the file; VACUUM once the freelist grows past 2 MiB.
const VACUUM_AFTER_FREE_PAGES: i64 = 512;

const PAGE_CACHE_KIB: i64 = 512;

// Integer floor division so SQL bucketing matches floor_to_grid's div_euclid for negative timestamps.
const BUCKET_START_SQL: &str = "((ts + ?4) / ?3 - (((ts + ?4) % ?3) < 0)) * ?3 - ?4";

// Summing `n` over the surviving raw window replaces a per-row LEFT JOIN scan at every open; an
// existing rollup minute keeps its `n`, so this can only over-report coverage by one partial minute.
const COVERAGE_PROBE_SQL: &str = "SELECT COALESCE(
             (SELECT SUM(n) FROM samples_1m
              WHERE ts >= (SELECT MIN(ts) FROM samples) / 60 * 60),
             0
         ) < (SELECT COUNT(*) FROM samples)";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub ts: i64,
    pub cpu: f64,
    pub mem: f64,
}

// `clock_skew` is (now, newest stored ts) when a forward wall-clock jump suppressed the round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: usize,
    pub clock_skew: Option<(i64, i64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyDatabase;

impl std::fmt::Display for EmptyDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("history database is empty")
    }
}

impl std::error::Error for EmptyDatabase {}

pub struct Store {
    conn: Connection,
    rollup_ready: bool,
    vacuum_pending: Cell<bool>,
}

pub fn floor_to_grid(ts: i64, step: i64, offset: i64) -> i64 {
    let step = step.max(1);
    (ts + offset).div_euclid(step) * step - offset
}

pub(crate) fn is_corruption_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<rusqlite::Error>().is_some_and(|err| {
            matches!(
                err,
                rusqlite::Error::SqliteFailure(code, _)
                    if matches!(
                        code.code,
                        ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
                    )
            )
        })
    })
}

fn rollup_backfill_needed(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(COVERAGE_PROBE_SQL, [], |row| row.get(0))
}

impl Store {
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -PAGE_CACHE_KIB)?;
        let rollup_ready = Self::init_schema(&mut conn)?;
        let _ = conn.execute_batch("PRAGMA shrink_memory");
        Ok(Self {
            conn,
            rollup_ready,
            vacuum_pending: Cell::new(false),
        })
    }

    pub fn open_read_only(path: &std::path::Path) -> anyhow::Result<Self> {
        use rusqlite::OpenFlags;
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() > 0 => {}
            Ok(_) => anyhow::bail!(EmptyDatabase),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!(EmptyDatabase)
            }
            Err(error) => return Err(error.into()),
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "cache_size", -PAGE_CACHE_KIB)?;
        let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "history database schema {version} is newer than supported {SCHEMA_VERSION}"
            );
        }
        Ok(Self {
            conn,
            rollup_ready: version == SCHEMA_VERSION,
            vacuum_pending: Cell::new(false),
        })
    }

    pub fn rollup_ready(&self) -> bool {
        self.rollup_ready
    }

    fn init_schema(conn: &mut Connection) -> anyhow::Result<bool> {
        let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "history database schema {version} is newer than supported {SCHEMA_VERSION}"
            );
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS samples (
                 ts  INTEGER PRIMARY KEY,
                 cpu REAL NOT NULL,
                 mem REAL NOT NULL
             );
             -- `ts` is an INTEGER PRIMARY KEY (a rowid alias) and is already
             -- indexed by the table itself; the old redundant index is dropped
             -- from databases created by earlier versions.
             DROP INDEX IF EXISTS idx_samples_ts;
             CREATE TABLE IF NOT EXISTS samples_1m (
                 ts      INTEGER PRIMARY KEY,
                 cpu_sum REAL NOT NULL,
                 mem_sum REAL NOT NULL,
                 n       INTEGER NOT NULL
             );",
        )?;
        let has_raw: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM samples)", [], |row| {
            row.get(0)
        })?;
        if !has_raw {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            return Ok(true);
        }
        let needs_backfill = if version < SCHEMA_VERSION {
            true
        } else {
            rollup_backfill_needed(conn)?
        };
        if !needs_backfill {
            return Ok(true);
        }
        conn.busy_timeout(Duration::from_secs(10))?;
        let backfill = (|| -> anyhow::Result<()> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT OR IGNORE INTO samples_1m (ts, cpu_sum, mem_sum, n)
                 SELECT (ts / 60) * 60, SUM(cpu), SUM(mem), COUNT(*)
                 FROM samples s
                 WHERE NOT EXISTS (
                     SELECT 1 FROM samples_1m b WHERE b.ts = (s.ts / 60) * 60
                 )
                 GROUP BY 1",
                [],
            )?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            Ok(())
        })();
        conn.busy_timeout(Duration::from_millis(250))?;
        Ok(backfill.is_ok())
    }

    #[cfg(test)]
    fn in_memory() -> anyhow::Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        let rollup_ready = Self::init_schema(&mut conn)?;
        Ok(Self {
            conn,
            rollup_ready,
            vacuum_pending: Cell::new(false),
        })
    }

    pub fn insert(&self, ts: i64, cpu: f64, mem: f64) -> anyhow::Result<()> {
        if !cpu.is_finite() || !mem.is_finite() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO samples (ts, cpu, mem) VALUES (?1, ?2, ?3)",
            params![ts, cpu, mem],
        )?;
        if inserted == 0 {
            return Ok(());
        }
        if self.rollup_ready {
            let minute = (ts / 60) * 60;
            tx.execute(
                "INSERT INTO samples_1m (ts, cpu_sum, mem_sum, n) VALUES (?1, ?2, ?3, 1)
                 ON CONFLICT(ts) DO UPDATE SET
                     cpu_sum = cpu_sum + excluded.cpu_sum,
                     mem_sum = mem_sum + excluded.mem_sum,
                     n       = n + excluded.n",
                params![minute, cpu, mem],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn prune(&self, now: i64, retention_days: u32) -> anyhow::Result<PruneReport> {
        let newest: Option<i64> =
            self.conn
                .query_row("SELECT MAX(ts) FROM samples_1m", [], |row| row.get(0))?;
        if let Some(newest) = newest {
            if now > newest + RAW_RETENTION_SECS {
                return Ok(PruneReport {
                    removed: 0,
                    clock_skew: Some((now, newest)),
                });
            }
        }
        let retention_cutoff = now - i64::from(retention_days) * 86_400;
        let mut removed = self.conn.execute(
            "DELETE FROM samples_1m WHERE ts < ?1",
            params![retention_cutoff],
        )?;
        let raw_cutoff = if self.rollup_ready {
            now - RAW_RETENTION_SECS
        } else {
            retention_cutoff
        };
        removed += self
            .conn
            .execute("DELETE FROM samples WHERE ts < ?1", params![raw_cutoff])?;
        if self.freelist_count()? >= VACUUM_AFTER_FREE_PAGES {
            self.vacuum_pending.set(true);
        }
        if self.vacuum_pending.get() {
            // VACUUM cannot run inside a transaction; the deletes above autocommit, so this is safe.
            if self.conn.execute_batch("VACUUM").is_ok() {
                self.vacuum_pending.set(false);
            }
        }
        Ok(PruneReport {
            removed,
            clock_skew: None,
        })
    }

    fn freelist_count(&self) -> anyhow::Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "freelist_count", |row| row.get(0))?)
    }

    pub fn buckets_offset(
        &self,
        from: i64,
        to: i64,
        bucket_secs: i64,
        offset_secs: i64,
    ) -> anyhow::Result<Vec<Bucket>> {
        let bucket = bucket_secs.max(1);
        if self.rollup_ready && bucket >= ROLLUP_MIN_BUCKET_SECS {
            self.buckets_from_rollup(from, to, bucket, offset_secs)
        } else {
            self.buckets_from_raw(from, to, bucket, offset_secs)
        }
    }

    fn buckets_from_raw(
        &self,
        from: i64,
        to: i64,
        bucket: i64,
        offset_secs: i64,
    ) -> anyhow::Result<Vec<Bucket>> {
        self.query_buckets(
            &format!(
                "SELECT {BUCKET_START_SQL} AS b, AVG(cpu), AVG(mem)
                 FROM samples
                 WHERE ts >= ?1 AND ts <= ?2
                 GROUP BY b
                 ORDER BY b"
            ),
            from,
            to,
            bucket,
            offset_secs,
        )
    }

    fn buckets_from_rollup(
        &self,
        from: i64,
        to: i64,
        bucket: i64,
        offset_secs: i64,
    ) -> anyhow::Result<Vec<Bucket>> {
        self.query_buckets(
            &format!(
                "SELECT {BUCKET_START_SQL} AS b,
                        SUM(cpu_sum) / SUM(n),
                        SUM(mem_sum) / SUM(n)
                 FROM samples_1m
                 WHERE ts >= ?1 AND ts <= ?2
                 GROUP BY b
                 ORDER BY b"
            ),
            from,
            to,
            bucket,
            offset_secs,
        )
    }

    fn query_buckets(
        &self,
        sql: &str,
        from: i64,
        to: i64,
        bucket: i64,
        offset_secs: i64,
    ) -> anyhow::Result<Vec<Bucket>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![from, to, bucket, offset_secs], |row| {
            Ok(Bucket {
                ts: row.get(0)?,
                cpu: row.get(1)?,
                mem: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
impl Store {
    fn row_count(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))
            .unwrap()
    }

    fn rollup_row_count(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM samples_1m", [], |row| row.get(0))
            .unwrap()
    }

    fn latest_row(&self) -> Option<(i64, f64, f64)> {
        self.conn
            .query_row(
                "SELECT ts, cpu, mem FROM samples ORDER BY ts DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_read_back() {
        let s = Store::in_memory().unwrap();
        assert!(s.rollup_ready());
        s.insert(100, 25.0, 60.0).unwrap();
        assert_eq!(s.latest_row(), Some((100, 25.0, 60.0)));
        assert_eq!(s.row_count(), 1);
        assert_eq!(s.rollup_row_count(), 1);
        let (ts, cpu_sum, mem_sum, n): (i64, f64, f64, i64) = s
            .conn
            .query_row("SELECT ts, cpu_sum, mem_sum, n FROM samples_1m", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap();
        assert_eq!((ts, cpu_sum, mem_sum, n), (60, 25.0, 60.0, 1));
    }

    #[test]
    fn wall_clock_rollback_samples_are_kept() {
        let s = Store::in_memory().unwrap();
        s.insert(1_700_000_000, 10.0, 20.0).unwrap();
        s.insert(1_699_999_000, 30.0, 40.0).unwrap();
        assert_eq!(s.row_count(), 2);
        let b = s
            .buckets_offset(1_699_998_990, 1_699_999_010, 10, 0)
            .unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].ts, 1_699_999_000);
        assert!((b[0].cpu - 30.0).abs() < 1e-9);
    }

    #[test]
    fn duplicate_timestamp_is_ignored() {
        let s = Store::in_memory().unwrap();
        s.insert(100, 10.0, 10.0).unwrap();
        s.insert(100, 90.0, 90.0).unwrap();
        assert_eq!(s.row_count(), 1);
        assert_eq!(s.latest_row(), Some((100, 10.0, 10.0)));
        let (cpu_sum, n): (f64, i64) = s
            .conn
            .query_row("SELECT cpu_sum, n FROM samples_1m", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((cpu_sum, n), (10.0, 1), "the minute average must not move");
    }

    #[test]
    fn nan_sample_is_skipped_not_error() {
        let s = Store::in_memory().unwrap();
        s.insert(100, f64::NAN, 50.0).unwrap();
        s.insert(101, 50.0, f64::NAN).unwrap();
        s.insert(102, f64::NAN, f64::NAN).unwrap();
        s.insert(104, f64::INFINITY, 50.0).unwrap();
        assert_eq!(s.row_count(), 0, "non-finite rows must not be stored");
        assert_eq!(s.rollup_row_count(), 0, "no rollup row either");
        s.insert(103, 50.0, 50.0).unwrap();
        assert_eq!(s.row_count(), 1);
        assert_eq!(s.rollup_row_count(), 1);
        assert_eq!(s.latest_row().unwrap().0, 103);
    }

    #[test]
    fn schema_has_no_redundant_timestamp_index() {
        let s = Store::in_memory().unwrap();
        let n: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_samples_ts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "ts is a rowid alias and needs no extra index");
    }

    #[test]
    fn prune_drops_old_raw_but_keeps_rollup_within_retention() {
        let s = Store::in_memory().unwrap();
        let now = 4_000_000_020i64;
        s.insert(now - 3_600, 0.5, 0.5).unwrap();
        s.insert(now - 2 * 86_400, 1.0, 1.0).unwrap();
        s.insert(now - 60 * 86_400, 2.0, 2.0).unwrap();
        let report = s.prune(now, 30).unwrap();
        assert_eq!(report.clock_skew, None);
        assert_eq!(
            report.removed, 3,
            "two raw rows past 24h and the 60-day rollup minute"
        );
        assert_eq!(s.row_count(), 1, "the recent raw row stays");
        assert_eq!(s.rollup_row_count(), 2);
        let b = s.buckets_offset(now - 30 * 86_400, now, 120, 0).unwrap();
        assert_eq!(b.len(), 2);
        assert!(
            (b[1].cpu - 0.5).abs() < 1e-9,
            "the surviving minute reads back"
        );
    }

    #[test]
    fn forward_clock_jump_past_retention_skips_the_prune_round() {
        let s = Store::in_memory().unwrap();
        let now = 1_700_000_000i64;
        s.insert(now - 120, 10.0, 20.0).unwrap();
        s.insert(now - 2 * 86_400, 30.0, 40.0).unwrap();
        let jumped = now + 400 * 86_400;
        let report = s.prune(jumped, 30).unwrap();
        assert_eq!(
            report.clock_skew,
            Some((jumped, floor_to_grid(now - 120, 60, 0))),
            "the jump must be reported, not acted on"
        );
        assert_eq!(report.removed, 0);
        assert_eq!(s.row_count(), 2, "no raw row may be deleted");
        assert_eq!(s.rollup_row_count(), 2, "no rollup row may be deleted");

        let recovered = s.prune(now, 30).unwrap();
        assert_eq!(recovered.clock_skew, None);
        assert_eq!(
            recovered.removed, 1,
            "the 2-day-old raw row prunes once the clock is sane"
        );
    }

    #[test]
    fn an_empty_store_prunes_without_a_skew_report() {
        let s = Store::in_memory().unwrap();
        let report = s.prune(1_700_000_000, 30).unwrap();
        assert_eq!(
            report,
            PruneReport {
                removed: 0,
                clock_skew: None
            }
        );
    }

    #[test]
    fn raw_only_fallback_keeps_the_full_retention_and_scans_it() {
        let mut s = Store::in_memory().unwrap();
        s.rollup_ready = false;
        let now = 4_000_000_020i64;
        s.insert(now - 2 * 86_400, 1.0, 2.0).unwrap();
        s.insert(now - 60 * 86_400, 3.0, 4.0).unwrap();
        assert_eq!(s.rollup_row_count(), 0);
        assert_eq!(
            s.prune(now, 30).unwrap().removed,
            1,
            "only the 60-day raw row"
        );
        assert_eq!(s.row_count(), 1, "raw keeps the full 30-day window");
        let b = s.buckets_offset(now - 3 * 86_400, now, 3600, 0).unwrap();
        assert_eq!(b.len(), 1, "wide buckets fall back to raw");
        assert!((b[0].cpu - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_large_freelist_arms_and_clears_the_one_shot_vacuum() {
        let s = Store::in_memory().unwrap();
        let tx = s.conn.unchecked_transaction().unwrap();
        {
            let mut stmt = tx
                .prepare("INSERT INTO samples (ts, cpu, mem) VALUES (?1, 1.0, 1.0)")
                .unwrap();
            for ts in 0..100_000i64 {
                stmt.execute(params![ts]).unwrap();
            }
        }
        tx.commit().unwrap();
        assert!(
            s.freelist_count().unwrap() == 0,
            "nothing is free before the delete"
        );
        let report = s.prune(4_000_000_020, 30).unwrap();
        assert_eq!(report.removed, 100_000);
        assert_eq!(s.row_count(), 0);
        assert!(
            !s.vacuum_pending.get(),
            "a successful one-shot VACUUM must clear the flag"
        );
        assert!(
            s.freelist_count().unwrap() < VACUUM_AFTER_FREE_PAGES,
            "the file gave its free pages back"
        );
    }

    #[test]
    fn a_small_delete_does_not_arm_the_vacuum() {
        let s = Store::in_memory().unwrap();
        let now = 1_700_000_000i64;
        s.insert(now - 60, 1.0, 1.0).unwrap();
        s.insert(now - 2 * 86_400, 1.0, 1.0).unwrap();
        let report = s.prune(now, 30).unwrap();
        assert_eq!(report.removed, 1);
        assert!(!s.vacuum_pending.get());
        assert!(s.freelist_count().unwrap() < VACUUM_AFTER_FREE_PAGES);
    }

    #[test]
    fn coverage_probe_flags_uncovered_minutes_without_scanning_every_row() {
        for (raw, rollup, expected) in [
            (vec![], vec![], false),
            (vec![60, 65, 120], vec![(60, 2), (120, 1)], false),
            (vec![60, 65, 120], vec![(120, 1)], true),
            (vec![60, 65, 120], vec![], true),
            (vec![60], vec![(60, 4)], false),
            (vec![60, 65, 120, 180], vec![(60, 3)], true),
            (vec![120], vec![(60, 2), (120, 1)], false),
            (vec![120], vec![(60, 200)], true),
        ] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(
                "CREATE TABLE samples (ts INTEGER PRIMARY KEY, cpu REAL NOT NULL, mem REAL NOT NULL);
                 CREATE TABLE samples_1m (
                     ts INTEGER PRIMARY KEY, cpu_sum REAL NOT NULL,
                     mem_sum REAL NOT NULL, n INTEGER NOT NULL
                 );",
            )
            .unwrap();
            for ts in &raw {
                conn.execute(
                    "INSERT INTO samples (ts, cpu, mem) VALUES (?1, 1.0, 1.0)",
                    params![ts],
                )
                .unwrap();
            }
            for (ts, n) in &rollup {
                conn.execute(
                    "INSERT INTO samples_1m (ts, cpu_sum, mem_sum, n) VALUES (?1, 1.0, 1.0, ?2)",
                    params![ts, n],
                )
                .unwrap();
            }
            assert_eq!(
                rollup_backfill_needed(&conn).unwrap(),
                expected,
                "raw {raw:?} rollup {rollup:?}"
            );
        }
    }

    #[test]
    fn open_read_only_reports_a_zero_byte_database_as_empty() {
        fn assert_empty(result: anyhow::Result<Store>) {
            match result {
                Ok(_) => panic!("an unreadable database must not open"),
                Err(error) => assert!(
                    error.downcast_ref::<EmptyDatabase>().is_some(),
                    "expected an EmptyDatabase error, got {error}"
                ),
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "sysvitals-store-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        assert_empty(Store::open_read_only(&dir.join("missing.db")));

        let zero = dir.join("zero.db");
        std::fs::write(&zero, []).unwrap();
        assert_empty(Store::open_read_only(&zero));

        let real = dir.join("real.db");
        Store::open(&real).unwrap().insert(60, 1.0, 1.0).unwrap();
        assert!(Store::open_read_only(&real).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sql_bucketing_matches_floor_to_grid_for_negative_timestamps() {
        let s = Store::in_memory().unwrap();
        let raw_ts = [-95i64, -5, 5];
        for ts in raw_ts {
            s.insert(ts, 1.0, 1.0).unwrap();
        }
        for offset in [0i64, 3, -7] {
            let mut expected: Vec<i64> = raw_ts
                .iter()
                .map(|ts| floor_to_grid(*ts, 10, offset))
                .collect();
            expected.sort_unstable();
            expected.dedup();
            let raw = s.buckets_from_raw(-100, 10, 10, offset).unwrap();
            let actual: Vec<i64> = raw.iter().map(|b| b.ts).collect();
            assert_eq!(actual, expected, "offset {offset}");
            let rolled = s.buckets_from_rollup(-200, 10, 60, offset).unwrap();
            for bucket in &rolled {
                assert_eq!(
                    bucket.ts,
                    floor_to_grid(bucket.ts, 60, offset),
                    "rollup bucket start is on the grid (offset {offset})"
                );
            }
        }
    }

    #[test]
    fn buckets_average_within_window() {
        let s = Store::in_memory().unwrap();
        s.insert(0, 10.0, 20.0).unwrap();
        s.insert(5, 30.0, 40.0).unwrap();
        s.insert(12, 50.0, 60.0).unwrap();

        let b = s.buckets_offset(0, 20, 10, 0).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].ts, 0);
        assert_eq!(b[0].cpu, 20.0);
        assert_eq!(b[0].mem, 30.0);
        assert_eq!(b[1].ts, 10);
        assert_eq!(b[1].cpu, 50.0);
        assert_eq!(b[1].mem, 60.0);

        let wide = s.buckets_offset(0, 20, 60, 0).unwrap();
        assert_eq!(wide.len(), 1);
        assert_eq!(wide[0].ts, 0);
        assert_eq!(wide[0].cpu, 30.0);
        assert_eq!(wide[0].mem, 40.0);
    }

    #[test]
    fn buckets_respect_range_bounds() {
        let s = Store::in_memory().unwrap();
        for i in 0..30 {
            s.insert(i, i as f64, i as f64).unwrap();
        }
        let b = s.buckets_offset(10, 19, 5, 0).unwrap();
        assert!(b.iter().all(|x| x.ts >= 10 && x.ts <= 19));
    }

    #[test]
    fn buckets_align_to_epoch_by_default() {
        let s = Store::in_memory().unwrap();
        s.insert(3, 10.0, 10.0).unwrap();
        s.insert(13, 20.0, 20.0).unwrap();
        s.insert(23, 30.0, 30.0).unwrap();
        let b = s.buckets_offset(0, 30, 10, 0).unwrap();
        let starts: Vec<i64> = b.iter().map(|x| x.ts).collect();
        assert_eq!(starts, vec![0, 10, 20]);
    }

    #[test]
    fn buckets_offset_shifts_boundary_to_wall_clock() {
        let s = Store::in_memory().unwrap();
        let utc_day_1_0200 = 86_400 + 2 * 3600;
        s.insert(utc_day_1_0200, 42.0, 42.0).unwrap();
        let offset = s.buckets_offset(0, 200_000, 86_400, 28_800).unwrap();
        assert_eq!(offset[0].ts, 57_600);
        let plain = s.buckets_offset(0, 200_000, 86_400, 0).unwrap();
        assert_eq!(plain[0].ts, 86_400);
    }

    #[test]
    fn wide_buckets_match_raw_aggregation() {
        let s = Store::in_memory().unwrap();
        let base = 86_400i64 * 20_000;
        for i in 0..1800 {
            let ts = base + i * 2;
            s.insert(ts, (i % 100) as f64, ((i * 7) % 100) as f64)
                .unwrap();
        }
        for bucket in [120, 600, 3600] {
            let raw = s.buckets_from_raw(base, base + 3598, bucket, 0).unwrap();
            let rolled = s.buckets_from_rollup(base, base + 3598, bucket, 0).unwrap();
            assert_eq!(raw.len(), rolled.len(), "bucket {bucket} count");
            for (r, m) in raw.iter().zip(&rolled) {
                assert_eq!(r.ts, m.ts, "bucket {bucket} start");
                assert!((r.cpu - m.cpu).abs() < 1e-9, "bucket {bucket} cpu");
                assert!((r.mem - m.mem).abs() < 1e-9, "bucket {bucket} mem");
            }
            let routed = s.buckets_offset(base, base + 3598, bucket, 0).unwrap();
            assert_eq!(routed, rolled);
        }
    }

    #[test]
    fn wide_buckets_read_rollup_and_narrow_read_raw() {
        let s = Store::in_memory().unwrap();
        s.insert(120, 10.0, 20.0).unwrap();
        s.conn.execute("DELETE FROM samples", []).unwrap();
        let wide = s.buckets_offset(0, 179, 120, 0).unwrap();
        assert_eq!(wide.len(), 1);
        assert_eq!(wide[0].ts, 120);
        assert_eq!(wide[0].cpu, 10.0);
        assert!(s.buckets_offset(0, 179, 10, 0).unwrap().is_empty());
    }

    #[test]
    fn migration_backfills_rollup_once() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (ts INTEGER PRIMARY KEY, cpu REAL NOT NULL, mem REAL NOT NULL);
             INSERT INTO samples (ts, cpu, mem) VALUES
                 (60, 10.0, 20.0),
                 (65, 30.0, 40.0),
                 (120, 5.0, 6.0);",
        )
        .unwrap();
        assert!(Store::init_schema(&mut conn).unwrap());
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let rows: Vec<(i64, f64, f64, i64)> = {
            let mut stmt = conn
                .prepare("SELECT ts, cpu_sum, mem_sum, n FROM samples_1m ORDER BY ts")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(rows, vec![(60, 40.0, 60.0, 2), (120, 5.0, 6.0, 1)]);
        assert!(Store::init_schema(&mut conn).unwrap());
        let total: i64 = conn
            .query_row("SELECT SUM(n) FROM samples_1m", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 3);
    }

    #[test]
    fn version_two_with_uncovered_raw_rows_is_healed() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (ts INTEGER PRIMARY KEY, cpu REAL NOT NULL, mem REAL NOT NULL);
             CREATE TABLE samples_1m (
                 ts INTEGER PRIMARY KEY, cpu_sum REAL NOT NULL,
                 mem_sum REAL NOT NULL, n INTEGER NOT NULL
             );
             INSERT INTO samples (ts, cpu, mem) VALUES (60, 10.0, 20.0), (65, 30.0, 40.0);
             INSERT INTO samples_1m (ts, cpu_sum, mem_sum, n) VALUES (120, 5.0, 6.0, 1);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();
        assert!(Store::init_schema(&mut conn).unwrap());
        let rows: Vec<(i64, f64, f64, i64)> = {
            let mut stmt = conn
                .prepare("SELECT ts, cpu_sum, mem_sum, n FROM samples_1m ORDER BY ts")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(rows, vec![(60, 40.0, 60.0, 2), (120, 5.0, 6.0, 1)]);
        assert!(Store::init_schema(&mut conn).unwrap());
        let total: i64 = conn
            .query_row("SELECT SUM(n) FROM samples_1m", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 3);
    }

    #[test]
    fn healing_does_not_replace_an_existing_partial_rollup_minute() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (ts INTEGER PRIMARY KEY, cpu REAL NOT NULL, mem REAL NOT NULL);
             CREATE TABLE samples_1m (
                 ts INTEGER PRIMARY KEY, cpu_sum REAL NOT NULL,
                 mem_sum REAL NOT NULL, n INTEGER NOT NULL
             );
             INSERT INTO samples (ts, cpu, mem) VALUES (60, 10.0, 20.0), (65, 30.0, 40.0);
             INSERT INTO samples_1m (ts, cpu_sum, mem_sum, n) VALUES (60, 75.0, 90.0, 3);
             INSERT INTO samples (ts, cpu, mem) VALUES (120, 5.0, 6.0), (180, 7.0, 8.0);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();
        assert!(Store::init_schema(&mut conn).unwrap());
        let rows: Vec<(i64, f64, f64, i64)> = {
            let mut stmt = conn
                .prepare("SELECT ts, cpu_sum, mem_sum, n FROM samples_1m ORDER BY ts")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(
            rows,
            vec![(60, 75.0, 90.0, 3), (120, 5.0, 6.0, 1), (180, 7.0, 8.0, 1)],
            "the existing partial minute must stay authoritative"
        );
    }

    #[test]
    fn future_schema_is_rejected_instead_of_assumed_compatible() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let err = Store::init_schema(&mut conn).unwrap_err();
        assert!(
            err.to_string().contains("newer than supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn fresh_schema_marks_rollup_ready() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(Store::init_schema(&mut conn).unwrap());
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM samples_1m", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn empty_store_returns_empty() {
        let s = Store::in_memory().unwrap();
        assert!(s.latest_row().is_none());
        assert!(s.buckets_offset(0, 100, 10, 0).unwrap().is_empty());
        assert!(s.buckets_offset(0, 100, 3600, 3600).unwrap().is_empty());
        assert_eq!(s.row_count(), 0);
    }

    #[test]
    fn connections_use_the_small_page_cache() {
        let dir = std::env::temp_dir().join(format!(
            "sysvitals-store-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        let store = Store::open(&path).unwrap();
        let cache: i64 = store
            .conn
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .unwrap();
        assert_eq!(cache, -PAGE_CACHE_KIB, "writer page cache is bounded");
        drop(store);

        let reader = Store::open_read_only(&path).unwrap();
        let cache: i64 = reader
            .conn
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .unwrap();
        assert_eq!(cache, -PAGE_CACHE_KIB, "reader page cache is bounded");
        drop(reader);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn floor_to_grid_snaps_down_to_the_wall_clock_grid() {
        assert_eq!(floor_to_grid(3_725, 60, 0), 3_720);
        assert_eq!(floor_to_grid(3_660, 60, 0), 3_660);
        assert_eq!(floor_to_grid(93_600, 86_400, 28_800), 57_600);
        assert_eq!(floor_to_grid(-5, 10, 0), -10);
        assert_eq!(floor_to_grid(42, 0, 0), 42);
    }
}
