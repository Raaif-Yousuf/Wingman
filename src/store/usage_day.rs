//! The `usage_day` table: per-day spend, replacing the old `spend.json`.

use rusqlite::params;

use super::{Store, StoreError};
use crate::provider::Usage;
use crate::usage::DayTotal;

/// Newest days kept.
pub const KEEP_DAYS: usize = 60;

impl Store {
    /// Adds one request to the day's row for this provider and model, then
    /// drops the oldest days past [`KEEP_DAYS`]. `cost` is `None` for an
    /// unpriced request.
    pub fn record_usage(
        &self,
        date: &str,
        provider: &str,
        model: &str,
        cost: Option<f64>,
        usage: Option<&Usage>,
    ) -> Result<(), StoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let (inp, out, cw, cr) = match usage {
            Some(u) => (
                Some(i64::from(u.input_tokens)),
                Some(i64::from(u.output_tokens)),
                Some(i64::from(u.cache_write_tokens)),
                Some(i64::from(u.cache_read_tokens)),
            ),
            None => (None, None, None, None),
        };
        tx.execute(
            "INSERT INTO usage_day
               (date, provider, model, requests, unpriced,
                input_tokens, output_tokens, cache_write_tokens, cache_read_tokens, usd)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (date, provider, model) DO UPDATE SET
               requests = requests + 1,
               unpriced = unpriced + excluded.unpriced,
               input_tokens = CASE WHEN excluded.input_tokens IS NULL THEN input_tokens
                                   ELSE COALESCE(input_tokens, 0) + excluded.input_tokens END,
               output_tokens = CASE WHEN excluded.output_tokens IS NULL THEN output_tokens
                                    ELSE COALESCE(output_tokens, 0) + excluded.output_tokens END,
               cache_write_tokens = CASE WHEN excluded.cache_write_tokens IS NULL THEN cache_write_tokens
                                         ELSE COALESCE(cache_write_tokens, 0) + excluded.cache_write_tokens END,
               cache_read_tokens = CASE WHEN excluded.cache_read_tokens IS NULL THEN cache_read_tokens
                                        ELSE COALESCE(cache_read_tokens, 0) + excluded.cache_read_tokens END,
               usd = usd + excluded.usd",
            params![
                date,
                provider,
                model,
                i64::from(cost.is_none()),
                inp,
                out,
                cw,
                cr,
                cost.unwrap_or(0.0)
            ],
        )?;
        tx.execute(
            "DELETE FROM usage_day WHERE date NOT IN
               (SELECT DISTINCT date FROM usage_day ORDER BY date DESC LIMIT ?1)",
            [KEEP_DAYS as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The totals for one local date, or `None` when nothing was recorded.
    pub fn day_total(&self, date: &str) -> Result<Option<DayTotal>, StoreError> {
        let conn = self.lock();
        let (n, usd, requests, unpriced): (i64, f64, i64, i64) = conn.query_row(
            "SELECT count(*), COALESCE(sum(usd), 0), COALESCE(sum(requests), 0),
                    COALESCE(sum(unpriced), 0)
             FROM usage_day WHERE date = ?1",
            [date],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(DayTotal {
            usd,
            requests: u32::try_from(requests).unwrap_or(u32::MAX),
            unpriced: u32::try_from(unpriced).unwrap_or(u32::MAX),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(i: u32, o: u32) -> Usage {
        Usage {
            input_tokens: i,
            output_tokens: o,
            ..Default::default()
        }
    }

    #[test]
    fn a_day_accumulates_across_requests_and_providers() {
        let s = Store::in_memory();
        s.record_usage(
            "2026-10-15",
            "anthropic",
            "m",
            Some(0.25),
            Some(&usage(10, 5)),
        )
        .unwrap();
        s.record_usage(
            "2026-10-15",
            "anthropic",
            "m",
            Some(0.5),
            Some(&usage(20, 5)),
        )
        .unwrap();
        s.record_usage("2026-10-15", "openai", "x", None, None)
            .unwrap();

        let d = s.day_total("2026-10-15").unwrap().unwrap();
        assert_eq!(d.requests, 3);
        assert_eq!(d.unpriced, 1);
        assert!((d.usd - 0.75).abs() < 1e-9);

        let (inp, out): (i64, i64) = s
            .lock()
            .query_row(
                "SELECT input_tokens, output_tokens FROM usage_day WHERE provider = 'anthropic'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((inp, out), (30, 10));
    }

    #[test]
    fn another_day_has_no_total() {
        let s = Store::in_memory();
        s.record_usage("2026-10-15", "a", "m", Some(1.0), None)
            .unwrap();
        assert_eq!(s.day_total("2026-10-14").unwrap(), None);
    }

    #[test]
    fn only_the_newest_sixty_days_are_kept() {
        let s = Store::in_memory();
        for day in 1..=62 {
            let date = format!("2026-{:02}-{:02}", 1 + (day - 1) / 28, 1 + (day - 1) % 28);
            s.record_usage(&date, "a", "m", Some(0.01), None).unwrap();
        }
        let n: i64 = s
            .lock()
            .query_row("SELECT count(DISTINCT date) FROM usage_day", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n as usize, KEEP_DAYS);
        assert_eq!(s.day_total("2026-01-01").unwrap(), None);
        assert!(s.day_total("2026-03-06").unwrap().is_some());
    }

    #[test]
    fn concurrent_writers_all_land() {
        let s = std::sync::Arc::new(Store::in_memory());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        s.record_usage("2026-10-15", "a", "m", Some(0.01), None)
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(s.day_total("2026-10-15").unwrap().unwrap().requests, 200);
    }
}
