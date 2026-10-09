//! What calls cost, and the ledger of what has been spent.
//!
//! Every model call appends one line to `spend/YYYY-MM.jsonl`:
//!
//! ```json
//! {"at":"2026-10-09T12:00:00Z","session":"0199…","connection":"openrouter","model":"vendor/model-id","usage":{"input":1200,"output":80,"cached":1000},"cost":0.0042}
//! ```
//!
//! A cost is known when the provider reported one for the call, or when the
//! model has a price. Otherwise it is unknown, and it stays unknown: an
//! unpriced call is counted beside the total, never added to it as zero.
//! Months and days are UTC.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

use crate::files::{SHARED_DIR, ensure_dir};
use crate::llm::Model;
use crate::{Result, jsonl};

/// The directory under the state directory that holds the ledger.
pub const SPEND_DIR: &str = "spend";

/// Token counts for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// The whole prompt, including what was read from or written to a cache.
    pub input: u64,
    pub output: u64,
    /// The part of the prompt read from the provider's cache.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cached: u64,
    /// The part of the prompt written to the provider's cache.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A model's price, in dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    /// Prompt tokens read from the cache. Charged as input when not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Prompt tokens written to the cache. Charged as input when not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

impl Rates {
    /// A model that costs nothing, such as one on your own hardware.
    pub const FREE: Self = Self {
        input: 0.0,
        output: 0.0,
        cache_read: None,
        cache_write: None,
    };

    /// Whether every rate is a real, non-negative number. A price list is
    /// someone else's data; a rate that is not is treated as no price.
    pub fn is_usable(&self) -> bool {
        [
            Some(self.input),
            Some(self.output),
            self.cache_read,
            self.cache_write,
        ]
        .into_iter()
        .flatten()
        .all(|rate| rate.is_finite() && rate >= 0.0)
    }

    /// The cost of `usage` in dollars.
    pub fn cost(&self, usage: &Usage) -> f64 {
        let cached = usage.cached.min(usage.input);
        let written = usage.cache_write.min(usage.input - cached);
        let plain = usage.input - cached - written;
        let per_token = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
        per_token(plain, self.input)
            + per_token(cached, self.cache_read.unwrap_or(self.input))
            + per_token(written, self.cache_write.unwrap_or(self.input))
            + per_token(usage.output, self.output)
    }
}

/// The prices Tiphys knows: the owner's overrides, then what each
/// connection's model list said.
#[derive(Debug, Clone, Default)]
pub struct PriceBook {
    overrides: BTreeMap<String, Rates>,
    listed: BTreeMap<(String, String), Rates>,
}

impl PriceBook {
    /// A book holding the owner's prices from `[pricing]`, by model id.
    pub fn new(overrides: BTreeMap<String, Rates>) -> Self {
        Self {
            overrides,
            listed: BTreeMap::new(),
        }
    }

    /// Takes the prices from a connection's model list.
    pub fn learn(&mut self, connection: &str, models: &[Model]) {
        for model in models {
            if let Some(rates) = model.rates.filter(Rates::is_usable) {
                self.listed
                    .insert((connection.to_string(), model.id.clone()), rates);
            }
        }
    }

    /// The price of a model on a connection, if it is known. A connection on
    /// the owner's own hardware (`local`) is free unless they say otherwise.
    pub fn rates(&self, connection: &str, model: &str, local: bool) -> Option<Rates> {
        self.overrides
            .get(model)
            .or_else(|| {
                self.listed
                    .get(&(connection.to_string(), model.to_string()))
            })
            .copied()
            .or(local.then_some(Rates::FREE))
    }
}

/// The cost of one call: the provider's own figure when it gave one,
/// otherwise the price times the usage, otherwise unknown.
pub fn cost_of(reported: Option<f64>, rates: Option<Rates>, usage: Option<&Usage>) -> Option<f64> {
    reported
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
        .or_else(|| Some(rates?.cost(usage?)))
        // To a billionth of a dollar, so the ledger holds 0.00112 and not
        // 0.0011200000000000001.
        .map(|cost| (cost * 1e9).round() / 1e9)
}

/// One line of the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Charge {
    pub at: DateTime<Utc>,
    pub session: String,
    pub connection: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Dollars, or nothing when the price is not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

/// Writes a charge to the ledger for its month.
pub fn record(home: &Path, charge: &Charge) -> Result<()> {
    let dir = home.join(SPEND_DIR);
    ensure_dir(&dir, SHARED_DIR)?;
    jsonl::append(&month_file(home, charge.at), charge)
}

fn month_file(home: &Path, at: DateTime<Utc>) -> PathBuf {
    home.join(SPEND_DIR)
        .join(format!("{:04}-{:02}.jsonl", at.year(), at.month()))
}

/// What a set of calls came to.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Totals {
    /// Dollars, over the calls whose cost is known.
    pub cost: f64,
    pub calls: u32,
    /// Calls whose cost is not known and is not in `cost`.
    pub unpriced: u32,
}

impl Totals {
    fn add(&mut self, charge: &Charge) {
        self.calls += 1;
        match charge.cost {
            Some(cost) => self.cost += cost,
            None => self.unpriced += 1,
        }
    }
}

impl std::fmt::Display for Totals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let known = self.calls - self.unpriced;
        match (known, self.unpriced) {
            (_, 0) => write!(f, "{}", dollars(Some(self.cost))),
            (0, _) => write!(f, "{}", dollars(None)),
            (_, 1) => write!(
                f,
                "{} and 1 call at an unknown price",
                dollars(Some(self.cost))
            ),
            (_, n) => write!(
                f,
                "{} and {n} calls at an unknown price",
                dollars(Some(self.cost))
            ),
        }
    }
}

/// Today's spending and this month's, as of `now`.
pub fn totals_at(home: &Path, now: DateTime<Utc>) -> Result<(Totals, Totals)> {
    let charges: Vec<Charge> = jsonl::read(&month_file(home, now))?;
    let (mut today, mut month) = (Totals::default(), Totals::default());
    for charge in &charges {
        month.add(charge);
        if charge.at.date_naive() == now.date_naive() {
            today.add(charge);
        }
    }
    Ok((today, month))
}

/// Today's spending and this month's.
pub fn totals(home: &Path) -> Result<(Totals, Totals)> {
    totals_at(home, Utc::now())
}

/// A cost for display. An unknown cost is `$?.??`, never `$0.00`. Under a
/// cent, four decimals, so a small call does not read as free.
pub fn dollars(cost: Option<f64>) -> String {
    match cost {
        None => "$?.??".into(),
        Some(cost) if cost > 0.0 && cost < 0.01 => format!("${cost:.4}"),
        // `max` turns a negative zero into a plain one.
        Some(cost) => format!("${:.2}", cost.max(0.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn rates(input: f64, output: f64) -> Rates {
        Rates {
            input,
            output,
            cache_read: None,
            cache_write: None,
        }
    }

    fn usage(input: u64, output: u64, cached: u64, cache_write: u64) -> Usage {
        Usage {
            input,
            output,
            cached,
            cache_write,
        }
    }

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0).unwrap()
    }

    fn charge(when: DateTime<Utc>, cost: Option<f64>) -> Charge {
        Charge {
            at: when,
            session: "s1".into(),
            connection: "openrouter".into(),
            model: "vendor/model".into(),
            usage: Some(usage(1000, 100, 0, 0)),
            cost,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn cached_tokens_are_charged_at_their_own_rates_when_there_are_any() {
        let plain = rates(3.0, 15.0);
        let with_cache = Rates {
            cache_read: Some(0.3),
            cache_write: Some(3.75),
            ..plain
        };
        let cases = [
            (plain, usage(1_000_000, 0, 0, 0), 3.0),
            (plain, usage(0, 1_000_000, 0, 0), 15.0),
            // No cache rates: cached tokens cost what input does.
            (plain, usage(1_000_000, 0, 400_000, 100_000), 3.0),
            (
                with_cache,
                usage(1_000_000, 100_000, 400_000, 100_000),
                0.5 * 3.0 + 0.4 * 0.3 + 0.1 * 3.75 + 0.1 * 15.0,
            ),
            // A provider that reports more cached than prompt is not owed a refund.
            (with_cache, usage(100, 0, 500, 500), 100.0 * 0.3 / 1e6),
        ];
        for (rates, usage, expected) in cases {
            let cost = rates.cost(&usage);
            assert!(close(cost, expected), "{usage:?}: {cost} != {expected}");
        }
    }

    #[test]
    fn a_price_comes_from_the_owner_then_the_list_then_nowhere() {
        let listed = |id: &str, rates: Option<Rates>| Model {
            id: id.into(),
            context: None,
            rates,
            tools: None,
        };
        let mut book = PriceBook::new(BTreeMap::from([("mine".to_string(), rates(1.0, 2.0))]));
        book.learn(
            "openrouter",
            &[
                listed("mine", Some(rates(9.0, 9.0))),
                listed("listed", Some(rates(3.0, 15.0))),
                listed("unlisted", None),
                listed("broken", Some(rates(f64::NAN, 1.0))),
                listed("negative", Some(rates(-1.0, 1.0))),
            ],
        );
        assert_eq!(
            book.rates("openrouter", "mine", false),
            Some(rates(1.0, 2.0))
        );
        assert_eq!(
            book.rates("openrouter", "listed", false),
            Some(rates(3.0, 15.0))
        );
        assert_eq!(book.rates("other", "listed", false), None);
        for unknown in ["unlisted", "broken", "negative", "never-heard-of"] {
            assert_eq!(book.rates("openrouter", unknown, false), None, "{unknown}");
        }
        // Your own hardware is free, unless you priced it.
        assert_eq!(book.rates("local", "anything", true), Some(Rates::FREE));
        assert_eq!(book.rates("local", "mine", true), Some(rates(1.0, 2.0)));
    }

    #[test]
    fn a_call_costs_what_the_provider_said_or_the_price_or_is_unknown() {
        let used = usage(1_000_000, 0, 0, 0);
        let price = Some(rates(3.0, 15.0));
        assert_eq!(cost_of(Some(0.5), price, Some(&used)), Some(0.5));
        assert_eq!(cost_of(None, price, Some(&used)), Some(3.0));
        assert_eq!(cost_of(Some(f64::NAN), price, Some(&used)), Some(3.0));
        assert_eq!(cost_of(None, None, Some(&used)), None);
        assert_eq!(cost_of(None, price, None), None);
        assert_eq!(cost_of(Some(-1.0), None, None), None);
    }

    #[test]
    fn the_ledger_adds_up_today_and_the_month_and_keeps_unknowns_apart() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            totals_at(home.path(), at(9, 12)).unwrap(),
            (Totals::default(), Totals::default())
        );
        record(home.path(), &charge(at(8, 23), Some(1.0))).unwrap();
        record(home.path(), &charge(at(9, 1), Some(0.25))).unwrap();
        record(home.path(), &charge(at(9, 2), None)).unwrap();
        // Another month is another file.
        let september = Utc.with_ymd_and_hms(2026, 9, 30, 23, 0, 0).unwrap();
        record(home.path(), &charge(september, Some(50.0))).unwrap();

        let (today, month) = totals_at(home.path(), at(9, 12)).unwrap();
        assert!(close(today.cost, 0.25) && today.calls == 2 && today.unpriced == 1);
        assert!(close(month.cost, 1.25) && month.calls == 3 && month.unpriced == 1);
        assert!(home.path().join("spend/2026-10.jsonl").is_file());
        assert!(home.path().join("spend/2026-09.jsonl").is_file());
    }

    #[test]
    fn money_is_shown_honestly() {
        let cases = [
            (Some(0.0), "$0.00"),
            (Some(-0.0), "$0.00"),
            (Some(0.0042), "$0.0042"),
            (Some(0.01), "$0.01"),
            (Some(12.345), "$12.35"),
            (None, "$?.??"),
        ];
        for (cost, expected) in cases {
            assert_eq!(dollars(cost), expected);
        }

        let totals = |cost, calls, unpriced| Totals {
            cost,
            calls,
            unpriced,
        };
        let cases = [
            (totals(0.0, 0, 0), "$0.00"),
            (totals(1.5, 3, 0), "$1.50"),
            (totals(0.0, 2, 2), "$?.??"),
            (totals(1.5, 3, 1), "$1.50 and 1 call at an unknown price"),
            (totals(1.5, 5, 2), "$1.50 and 2 calls at an unknown price"),
        ];
        for (totals, expected) in cases {
            assert_eq!(totals.to_string(), expected);
        }
    }
}
