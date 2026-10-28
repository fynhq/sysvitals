//! History storage layer (R12).
//!
//! SQLite is the **single source of truth** for historical data: the trend
//! chart queries straight from the database, so a month of history survives
//! process restarts. CSV is *not* a storage format — it is generated on
//! demand from the database when the user exports.
//!
//! Only CPU and memory percentages are stored (both 0–100). Network rates are
//! deliberately excluded: they are shown live on the indicator but never
//! persisted or charted.
//!
//! Storage is tiered so wide ranges never scan every raw point:
//! - `samples` keeps the raw two-second points for [`RAW_RETENTION_SECS`]
//!   (24 h), which is what the 5-minute and 1-hour views read.
//! - `samples_1m` keeps weighted one-minute aggregates (`sum`, `n`) for the
//!   whole retention window. Every wider range (1 day, 1 month, CSV exports)
//!   buckets at 60 s or more, so combining minute sums is numerically
//!   identical to averaging the raw points.
//!
//! The store keeps a rolling window (`retention_days`, default 30) and prunes
//! anything older. A database created before the rollup existed is backfilled
//! once on open, gated by `PRAGMA user_version`. If that backfill fails the
//! store runs in raw-only mode: wide queries scan the raw table and raw rows
//! are not pruned, so nothing is lost before a retry succeeds.

use std::cell::Cell;
use std::time::Duration;

use rusqlite::{params, Connection, TransactionBehavior};

/// How long raw two-second samples are kept. Must cover the widest window
/// that reads raw points: the 1-hour view starts up to one label step
/// (600 s) before `now - 3600`, so 24 h leaves a wide margin.
const RAW_RETENTION_SECS: i64 = 24 * 60 * 60;

/// Buckets at least this wide are served from the one-minute rollup; narrower
/// ones (the 5-minute and 1-hour views) read raw samples.
const ROLLUP_MIN_BUCKET_SECS: i64 = 60;

/// Value stored in `PRAGMA user_version`: 0 means the database predates the
/// one-minute rollup (or its backfill has not succeeded yet); 2 is the
/// current raw + rollup schema.
const SCHEMA_VERSION: i32 = 2;

/// A prune that removes at least this many rows is treated as a one-time
/// downsizing (the first prune of a pre-rollup database deletes weeks of raw
/// samples). SQLite only frees pages for reuse on `DELETE`, so without a
/// `VACUUM` the file would keep its old size forever.
const VACUUM_AFTER_ROWS: usize = 100_000;

/// One aggregated bucket for charting: the bucket's start timestamp plus the
/// average CPU and memory over the bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub ts: i64,
    pub cpu: f64,
    pub mem: f64,
}

/// Thin wrapper over a SQLite connection holding the history schema.
pub struct Store {
    conn: Connection,
    /// False when the one-time backfill of an old database failed. Wide
    /// buckets then fall back to the raw table and raw pruning stays off
    /// until a later open manages to finish the migration.
    rollup_ready: bool,
    /// Set after a bulk prune; the next successful `VACUUM` clears it.
    vacuum_pending: Cell<bool>,
}

/// Floor `ts` down to the nearest boundary of a wall-clock grid whose spacing
/// is `step` seconds and whose zero sits at UTC offset `offset` (0 for the
/// epoch/UTC-midnight grid, the machine's local offset for a local-midnight
/// grid). This is the same alignment [`Store::buckets_offset`] applies to
/// bucket starts, exposed standalone so chart code can align a query window
/// or floor a label timestamp onto the identical grid.
///
/// `div_euclid` keeps the floor semantics for negative timestamps (samples
/// before 1970 or negative offsets), where plain integer division truncating
/// toward zero would land on the wrong side of the boundary.
pub fn floor_to_grid(ts: i64, step: i64, offset: i64) -> i64 {
    let step = step.max(1);
    (ts + offset).div_euclid(step) * step - offset
}

/// Which of `len` evenly-spaced data points a chart should label, when it
/// wants exactly `count` labels spread from the first point to the last:
/// indices `round(k * (len - 1) / (count - 1))` for `k` in `0..count`. Fewer
/// points than labels returns every index; `count == 1` returns just the
/// first. Mirrors the shape the trend chart's axis uses so a caller can
/// decide, per point, whether its own label text will actually be painted.
pub fn spread_label_indices(len: usize, count: usize) -> Vec<usize> {
    match count {
        0 => Vec::new(),
        1 => {
            if len == 0 {
                Vec::new()
            } else {
                vec![0]
            }
        }
        count if count >= len => (0..len).collect(),
        count => (0..count)
            .map(|k| (k as f32 * (len - 1) as f32 / (count - 1) as f32).round() as usize)
            .collect(),
    }
}

impl Store {
    /// Open (or create) the database at `path` and ensure the schema exists.
    /// Uses WAL with `synchronous=NORMAL` so steady 1–3s inserts stay cheap and
    /// do not fsync on every write. A short busy timeout lets the history
    /// panel read while the indicator is writing.
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
        let rollup_ready = Self::init_schema(&mut conn)?;
        Ok(Self {
            conn,
            rollup_ready,
            vacuum_pending: Cell::new(false),
        })
    }

    /// Whether wide buckets are served from the one-minute rollup. When
    /// false (a failed one-time backfill), the store runs in raw-only mode:
    /// queries scan `samples` and raw pruning is skipped until a later open
    /// retries the migration. Callers log this once after opening.
    pub fn rollup_ready(&self) -> bool {
        self.rollup_ready
    }

    /// Create the schema when missing and bring a pre-rollup database up to
    /// [`SCHEMA_VERSION`] by backfilling `samples_1m` from the raw table in
    /// one transaction. Returns whether wide queries may use the rollup; a
    /// failed backfill is not fatal (raw-only fallback).
    fn init_schema(conn: &mut Connection) -> anyhow::Result<bool> {
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
        let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(true);
        }
        let has_raw: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM samples)", [], |row| {
            row.get(0)
        })?;
        if !has_raw {
            // Fresh database: the live insert path builds the rollup from
            // the first sample, so there is nothing to backfill.
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            return Ok(true);
        }
        // Old database with raw history but no rollup: aggregate it once.
        // Take an immediate transaction (with a longer wait than the steady
        // 250 ms) so a concurrently opening process waits for this backfill
        // instead of double-aggregating the same minutes.
        conn.busy_timeout(Duration::from_secs(10))?;
        let backfill = (|| -> anyhow::Result<()> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Re-check under the write lock: another process may have won.
            let version: i32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
            if version < SCHEMA_VERSION {
                tx.execute(
                    "INSERT OR REPLACE INTO samples_1m (ts, cpu_sum, mem_sum, n)
                     SELECT (ts / 60) * 60, SUM(cpu), SUM(mem), COUNT(*)
                     FROM samples
                     GROUP BY 1",
                    [],
                )?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            tx.commit()?;
            Ok(())
        })();
        conn.busy_timeout(Duration::from_millis(250))?;
        Ok(backfill.is_ok())
    }

    /// Open an in-memory database (used by tests).
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

    /// Insert one raw sample and fold it into its one-minute rollup in the
    /// same transaction. Returns `false` (a no-op) when either value is not a
    /// finite number: SQLite's `REAL NOT NULL` column rejects a NaN binding
    /// with a constraint error (verified empirically), and first/wake ticks
    /// legitimately carry NaN — skipping the row is correct, not an error.
    ///
    /// A duplicate timestamp is ignored (`INSERT OR IGNORE`) instead of
    /// overwriting the old row: the rollup already contains that sample, and
    /// adding the new value on top would skew the minute average. Re-runs
    /// with unchanged values behave exactly as before; only a clock
    /// adjustment landing on the same second keeps the first reading.
    pub fn insert(&self, ts: i64, cpu: f64, mem: f64) -> anyhow::Result<bool> {
        if !cpu.is_finite() || !mem.is_finite() {
            return Ok(false);
        }
        let tx = self.conn.unchecked_transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO samples (ts, cpu, mem) VALUES (?1, ?2, ?3)",
            params![ts, cpu, mem],
        )?;
        if inserted == 0 {
            return Ok(false);
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
        Ok(true)
    }

    /// Delete raw samples older than [`RAW_RETENTION_SECS`] and rollup
    /// minutes older than `retention_days` days before `now`. Returns the
    /// number of rows removed. In raw-only mode (a failed backfill) raw rows
    /// follow the full retention window instead of the short tier, so a later
    /// migration retry still has everything the user is entitled to and the
    /// table cannot grow without bound.
    ///
    /// A bulk removal (the first prune of a pre-rollup database deletes
    /// weeks of raw rows) arms a one-shot `VACUUM`, because deleting rows
    /// alone leaves the file at its old size. It is retried on later prunes
    /// if a concurrent reader keeps it busy.
    pub fn prune(&self, now: i64, retention_days: u32) -> anyhow::Result<usize> {
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
        if removed >= VACUUM_AFTER_ROWS {
            self.vacuum_pending.set(true);
        }
        if self.vacuum_pending.get() {
            // `VACUUM` cannot run inside a transaction; the deletes above
            // autocommit, so we are outside one here.
            if self.conn.execute_batch("VACUUM").is_ok() {
                self.vacuum_pending.set(false);
            }
        }
        Ok(removed)
    }

    /// Return the average CPU and memory per `bucket_secs`-wide bucket in
    /// `[from, to]`, ordered by bucket start, with bucket boundaries aligned
    /// to a wall clock whose UTC offset is `offset_secs` (0 for the epoch/UTC
    /// grid, the machine's local offset for local midnight/hour boundaries).
    /// Chart code that wants a clean local grid passes the local offset.
    ///
    /// Buckets at least [`ROLLUP_MIN_BUCKET_SECS`] wide are served from the
    /// one-minute rollup and weigh each minute by its sample count. This is
    /// numerically identical to averaging the raw points because every such
    /// bucket starts on a minute boundary (all real UTC offsets are whole
    /// minutes) and the rollup keeps exact sums. Narrower buckets read
    /// `samples`. Rollup-backed queries assume `to` is the current time, so
    /// the trailing partial minute contains exactly the samples up to now;
    /// all call sites floor `from` to a 60-second-wide grid.
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

    /// Aggregate raw samples (used for narrow buckets and the raw-only
    /// fallback). All real samples have positive unix seconds and offsets are
    /// bounded by ±14 hours, so `ts + off` stays positive and SQLite's
    /// integer division truncation ("toward zero") equals Euclidean floor
    /// here. No negative-side hazard.
    fn buckets_from_raw(
        &self,
        from: i64,
        to: i64,
        bucket: i64,
        offset_secs: i64,
    ) -> anyhow::Result<Vec<Bucket>> {
        let mut stmt = self.conn.prepare(
            "SELECT ((ts + ?4) / ?3) * ?3 - ?4 AS b, AVG(cpu), AVG(mem)
             FROM samples
             WHERE ts >= ?1 AND ts <= ?2
             GROUP BY b
             ORDER BY b",
        )?;
        let rows = stmt.query_map(params![from, to, bucket, offset_secs], |row| {
            Ok(Bucket {
                ts: row.get(0)?,
                cpu: row.get(1)?,
                mem: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Aggregate one-minute rollup rows with sample-count weighting. The
    /// minute start is floored onto the same wall-clock grid as raw samples,
    /// so bucket starts match the raw path exactly.
    fn buckets_from_rollup(
        &self,
        from: i64,
        to: i64,
        bucket: i64,
        offset_secs: i64,
    ) -> anyhow::Result<Vec<Bucket>> {
        let mut stmt = self.conn.prepare(
            "SELECT ((ts + ?4) / ?3) * ?3 - ?4 AS b,
                    SUM(cpu_sum) / SUM(n),
                    SUM(mem_sum) / SUM(n)
             FROM samples_1m
             WHERE ts >= ?1 AND ts <= ?2
             GROUP BY b
             ORDER BY b",
        )?;
        let rows = stmt.query_map(params![from, to, bucket, offset_secs], |row| {
            Ok(Bucket {
                ts: row.get(0)?,
                cpu: row.get(1)?,
                mem: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
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
    fn duplicate_timestamp_is_ignored() {
        let s = Store::in_memory().unwrap();
        assert!(s.insert(100, 10.0, 10.0).unwrap());
        // The second insert at the same second is a no-op: overwriting would
        // leave the raw row and its rollup contribution inconsistent.
        assert!(!s.insert(100, 90.0, 90.0).unwrap());
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
        // SQLite rejects a NaN binding into a `REAL NOT NULL` column, so the
        // store must skip non-finite samples (first/wake ticks) as a no-op
        // rather than error or store a poison row.
        let s = Store::in_memory().unwrap();
        assert!(!s.insert(100, f64::NAN, 50.0).unwrap());
        assert!(!s.insert(101, 50.0, f64::NAN).unwrap());
        assert!(!s.insert(102, f64::NAN, f64::NAN).unwrap());
        assert!(!s.insert(104, f64::INFINITY, 50.0).unwrap());
        assert_eq!(s.row_count(), 0, "non-finite rows must not be stored");
        assert_eq!(s.rollup_row_count(), 0, "no rollup row either");
        // A finite sample right after still inserts and becomes the latest.
        assert!(s.insert(103, 50.0, 50.0).unwrap());
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
        // 4e9 is a whole minute, keeping the bucket grid aligned below.
        let now = 4_000_000_020i64;
        s.insert(now - 2 * 86_400, 1.0, 1.0).unwrap(); // 2 days: raw dropped, rollup kept
        s.insert(now - 60 * 86_400, 2.0, 2.0).unwrap(); // 60 days: both dropped
        let removed = s.prune(now, 30).unwrap();
        assert_eq!(removed, 3, "two raw rows and the 60-day rollup minute");
        assert_eq!(s.row_count(), 0);
        assert_eq!(s.rollup_row_count(), 1);
        // The 2-day-old average is still available through the wide path.
        let b = s.buckets_offset(now - 30 * 86_400, now, 120, 0).unwrap();
        assert_eq!(b.len(), 1);
        assert!((b[0].cpu - 1.0).abs() < 1e-9);
    }

    #[test]
    fn raw_only_fallback_keeps_the_full_retention_and_scans_it() {
        let mut s = Store::in_memory().unwrap();
        // Simulate a failed backfill: no rollup writes, full-retention raw.
        s.rollup_ready = false;
        let now = 4_000_000_020i64;
        s.insert(now - 2 * 86_400, 1.0, 2.0).unwrap();
        s.insert(now - 60 * 86_400, 3.0, 4.0).unwrap();
        assert_eq!(s.rollup_row_count(), 0);
        assert_eq!(s.prune(now, 30).unwrap(), 1, "only the 60-day raw row");
        assert_eq!(s.row_count(), 1, "raw keeps the full 30-day window");
        let b = s.buckets_offset(now - 3 * 86_400, now, 3600, 0).unwrap();
        assert_eq!(b.len(), 1, "wide buckets fall back to raw");
        assert!((b[0].cpu - 1.0).abs() < 1e-9);
    }

    #[test]
    fn bulk_prune_arms_and_clears_the_one_shot_vacuum() {
        let s = Store::in_memory().unwrap();
        let tx = s.conn.unchecked_transaction().unwrap();
        {
            let mut stmt = tx
                .prepare("INSERT INTO samples (ts, cpu, mem) VALUES (?1, 1.0, 1.0)")
                .unwrap();
            for ts in 0..VACUUM_AFTER_ROWS as i64 {
                stmt.execute(params![ts]).unwrap();
            }
        }
        tx.commit().unwrap();
        // `now` far ahead: every raw row falls past the 24 h raw cut.
        let removed = s.prune(4_000_000_020, 30).unwrap();
        assert_eq!(removed, VACUUM_AFTER_ROWS);
        assert_eq!(s.row_count(), 0);
        assert!(
            !s.vacuum_pending.get(),
            "a successful one-shot VACUUM must clear the flag"
        );
    }

    #[test]
    fn buckets_average_within_window() {
        let s = Store::in_memory().unwrap();
        // Bucket width 10s. Two samples in bucket [0,10), one in [10,20).
        s.insert(0, 10.0, 20.0).unwrap();
        s.insert(5, 30.0, 40.0).unwrap();
        s.insert(12, 50.0, 60.0).unwrap();

        let b = s.buckets_offset(0, 20, 10, 0).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].ts, 0);
        assert_eq!(b[0].cpu, 20.0); // avg(10,30)
        assert_eq!(b[0].mem, 30.0); // avg(20,40)
        assert_eq!(b[1].ts, 10);
        assert_eq!(b[1].cpu, 50.0);
        assert_eq!(b[1].mem, 60.0);

        // The same samples through the rollup path (60 s bucket): all three
        // land in minute [0,60), so the minute average is their plain mean.
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
        // Only buckets whose rows fall inside [10,19].
        assert!(b.iter().all(|x| x.ts >= 10 && x.ts <= 19));
    }

    #[test]
    fn buckets_align_to_epoch_by_default() {
        let s = Store::in_memory().unwrap();
        // One sample near the start of each of three 10-wide buckets.
        s.insert(3, 10.0, 10.0).unwrap();
        s.insert(13, 20.0, 20.0).unwrap();
        s.insert(23, 30.0, 30.0).unwrap();
        let b = s.buckets_offset(0, 30, 10, 0).unwrap();
        let starts: Vec<i64> = b.iter().map(|x| x.ts).collect();
        // Epoch-aligned: boundaries are multiples of the bucket width.
        assert_eq!(starts, vec![0, 10, 20]);
    }

    #[test]
    fn buckets_offset_shifts_boundary_to_wall_clock() {
        let s = Store::in_memory().unwrap();
        // A +8h zone (28800 s) with a day-wide (86400) bucket: a sample at
        // 02:00 UTC belongs to the day that started at 16:00 UTC the prior
        // day, which is 00:00 local (+8) — the local-midnight grid.
        let utc_day_1_0200 = 86_400 + 2 * 3600; // 93600
        s.insert(utc_day_1_0200, 42.0, 42.0).unwrap();
        let offset = s.buckets_offset(0, 200_000, 86_400, 28_800).unwrap();
        // Boundary ts = ((93600 + 28800)/86400)*86400 - 28800 = 86400 - 28800
        // = 57600, i.e. the local midnight that opened this bucket.
        assert_eq!(offset[0].ts, 57_600);
        // Without the offset the same sample buckets to 86400 (UTC midnight).
        let plain = s.buckets_offset(0, 200_000, 86_400, 0).unwrap();
        assert_eq!(plain[0].ts, 86_400);
    }

    #[test]
    fn wide_buckets_match_raw_aggregation() {
        let s = Store::in_memory().unwrap();
        let base = 86_400i64 * 20_000; // whole minute, 600 s and day aligned
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
            // The public entry point routes wide buckets to the rollup.
            let routed = s.buckets_offset(base, base + 3598, bucket, 0).unwrap();
            assert_eq!(routed, rolled);
        }
    }

    #[test]
    fn wide_buckets_read_rollup_and_narrow_read_raw() {
        let s = Store::in_memory().unwrap();
        s.insert(120, 10.0, 20.0).unwrap();
        // Drop the raw row: only the minute aggregate remains.
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
        // Opening again is idempotent: the minutes are not aggregated twice.
        assert!(Store::init_schema(&mut conn).unwrap());
        let total: i64 = conn
            .query_row("SELECT SUM(n) FROM samples_1m", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 3);
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
    fn floor_to_grid_snaps_down_to_the_wall_clock_grid() {
        // Epoch grid (+0 offset): 3 725 s lands on the minute 3 720 s below.
        assert_eq!(floor_to_grid(3_725, 60, 0), 3_720);
        // Already on a boundary: unchanged.
        assert_eq!(floor_to_grid(3_660, 60, 0), 3_660);
        // Local grid (+8h): boundaries are local midnight multiples, i.e.
        // 57 600 = 16:00 UTC of the prior day is 00:00 local. 93 600 (02:00
        // UTC) floors to it — the same sample the buckets_offset test pins.
        assert_eq!(floor_to_grid(93_600, 86_400, 28_800), 57_600);
        // Negative timestamps floor downward, not toward zero: -5 with a
        // step of 10 belongs to the boundary -10, while truncating division
        // would wrongly give 0.
        assert_eq!(floor_to_grid(-5, 10, 0), -10);
        // A zero/garbage step is clamped to 1, never a division by zero.
        assert_eq!(floor_to_grid(42, 0, 0), 42);
    }

    #[test]
    fn spread_label_indices_pins_ends_and_stays_unique() {
        // 6 of 15 points: first and last are always labeled, indices advance.
        let idx = spread_label_indices(15, 6);
        assert_eq!(idx.len(), 6);
        assert_eq!(idx.first(), Some(&0));
        assert_eq!(idx.last(), Some(&14));
        let mut sorted = idx.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), idx.len(), "no index repeats");
        // Fewer points than labels: every point gets a label.
        assert_eq!(spread_label_indices(3, 6), vec![0, 1, 2]);
        // Degenerate inputs never panic and never invent an index.
        assert_eq!(spread_label_indices(0, 6), Vec::<usize>::new());
        assert_eq!(spread_label_indices(10, 0), Vec::<usize>::new());
        assert_eq!(spread_label_indices(10, 1), vec![0]);
        // Every returned index is inside the data range.
        for i in spread_label_indices(100, 6) {
            assert!(i < 100);
        }
    }
}
