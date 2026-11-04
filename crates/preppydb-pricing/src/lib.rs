//! PreppyDB dynamic pricing engine.
//!
//! Safety properties (all covered by tests):
//!
//! 1. **Bounded.** A rule declares `min_price_cents` and `max_price_cents`; a price can
//!    never leave that interval, no matter what the random stream does.
//! 2. **No negative prices.** Prices are integer cents and the validator rejects
//!    `min_price_cents < 1`. Discounts are clamped to the rule's floor (default 1 cent).
//! 3. **Reproducible.** All randomness comes from a caller-supplied seed
//!    (`PricingEngine::with_seed`), so a run can be replayed and audited exactly.
//! 4. **Version bound.** Offers carry the rule version they were created from. If the
//!    rule changes, the offer is stale and cannot be redeemed.
//! 5. **Immutable purchases.** [`PurchaseLedger`] only appends. Completed purchases keep
//!    the price that was actually charged; a later rule change cannot rewrite history.
//! 6. **Auditable.** Every accepted change is written to an append-only hash-chained
//!    audit log whose integrity can be verified.
//!
//! The engine has no dependency on Molly or any other application; it is usable as a
//! stand-alone library and through the PreppyDB HTTP API.

use preppydb_crypto::rng::ChaCha20Rng;
use preppydb_crypto::sha256::sha256;
use preppydb_json::Json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// --------------------------------------------------------------------------- errors

/// Pricing error type with stable codes for the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricingError {
    pub code: &'static str,
    pub message: String,
}

impl PricingError {
    fn new(code: &'static str, message: impl Into<String>) -> PricingError {
        PricingError {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PricingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for PricingError {}

pub type Result<T> = std::result::Result<T, PricingError>;

pub mod codes {
    pub const INVALID_RULE: &str = "invalid_price_rule";
    pub const OUT_OF_BOUNDS: &str = "price_out_of_bounds";
    pub const OFFER_STALE: &str = "offer_stale";
    pub const OFFER_EXPIRED: &str = "offer_expired";
    pub const OFFER_UNKNOWN: &str = "offer_unknown";
    pub const OFFER_REDEEMED: &str = "offer_already_redeemed";
    pub const AUDIT_INTEGRITY: &str = "audit_log_integrity";
    pub const DUPLICATE: &str = "duplicate_entry";
}

// --------------------------------------------------------------------------- rules

/// A percentage discount that is active inside a time window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscountRule {
    pub label: String,
    /// 1..=90 percent.
    pub percent: u32,
    /// Inclusive start (ms since epoch), `None` = always.
    pub starts_at: Option<i64>,
    /// Inclusive end (ms since epoch), `None` = never.
    pub ends_at: Option<i64>,
}

impl DiscountRule {
    pub fn is_active(&self, now_ms: i64) -> bool {
        if let Some(start) = self.starts_at {
            if now_ms < start {
                return false;
            }
        }
        if let Some(end) = self.ends_at {
            if now_ms > end {
                return false;
            }
        }
        true
    }

    fn validate(&self) -> Result<()> {
        if !(1..=90).contains(&self.percent) {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                format!("discount '{}' must be between 1 and 90 percent", self.label),
            ));
        }
        if let (Some(start), Some(end)) = (self.starts_at, self.ends_at) {
            if end < start {
                return Err(PricingError::new(
                    codes::INVALID_RULE,
                    format!("discount '{}' ends before it starts", self.label),
                ));
            }
        }
        Ok(())
    }

    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("label", Json::str(self.label.clone()))
            .with("percent", Json::uint(self.percent as u64))
            .with(
                "starts_at",
                self.starts_at.map(Json::int).unwrap_or(Json::Null),
            )
            .with("ends_at", self.ends_at.map(Json::int).unwrap_or(Json::Null))
    }

    pub fn from_json(value: &Json) -> Result<DiscountRule> {
        Ok(DiscountRule {
            label: value.get_str("label").unwrap_or("discount").to_string(),
            percent: value.get("percent").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            starts_at: value.get("starts_at").and_then(|v| v.as_i64()),
            ends_at: value.get("ends_at").and_then(|v| v.as_i64()),
        })
    }
}

/// A price rule for one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceRule {
    pub item_id: String,
    pub min_price_cents: i64,
    pub max_price_cents: i64,
    /// Maximum absolute change per pricing run, in cents.
    pub step_cents: i64,
    /// Price the item starts from; must be inside `[min, max]`.
    pub anchor_price_cents: i64,
    /// Hard lower bound after discounts (never below 1 cent).
    pub floor_price_cents: i64,
    pub discounts: Vec<DiscountRule>,
    /// Incremented every time a new price is accepted for this rule.
    pub version: u64,
    pub updated_at: i64,
    pub enabled: bool,
}

impl PriceRule {
    /// A rule with sane defaults: anchor in the middle, step = 5% of the range.
    pub fn new(
        item_id: &str,
        min_price_cents: i64,
        max_price_cents: i64,
        anchor: i64,
    ) -> PriceRule {
        let range = (max_price_cents - min_price_cents).max(1);
        PriceRule {
            item_id: item_id.to_string(),
            min_price_cents,
            max_price_cents,
            step_cents: (range / 20).max(1),
            anchor_price_cents: anchor,
            floor_price_cents: 1,
            discounts: Vec::new(),
            version: 1,
            updated_at: 0,
            enabled: true,
        }
    }

    /// Validate the rule. This is the only place that decides whether a rule is legal.
    pub fn validate(&self) -> Result<()> {
        if self.item_id.trim().is_empty() {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                "item_id must not be empty",
            ));
        }
        if self.min_price_cents < 1 {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                "min_price_cents must be at least 1 (prices are never zero or negative)",
            ));
        }
        if self.max_price_cents < self.min_price_cents {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                "max_price_cents must not be smaller than min_price_cents",
            ));
        }
        if self.anchor_price_cents < self.min_price_cents
            || self.anchor_price_cents > self.max_price_cents
        {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                "anchor_price_cents must lie inside [min_price_cents, max_price_cents]",
            ));
        }
        if self.step_cents < 0 {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                "step_cents must not be negative",
            ));
        }
        if self.floor_price_cents < 1 {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                "floor_price_cents must be at least 1",
            ));
        }
        if self.floor_price_cents > self.min_price_cents {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                "floor_price_cents must not exceed min_price_cents",
            ));
        }
        if self.max_price_cents > 100_000_000 {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                "max_price_cents above 100000000 is not supported (sanity bound)",
            ));
        }
        if self.version == 0 {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                "version must be >= 1",
            ));
        }
        for discount in &self.discounts {
            discount.validate()?;
        }
        Ok(())
    }

    /// Clamp an arbitrary value into the rule's bounds (used by everything that
    /// computes a price so the invariant cannot be bypassed).
    pub fn clamp(&self, price_cents: i64) -> i64 {
        price_cents.clamp(self.min_price_cents.max(1), self.max_price_cents.max(1))
    }

    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("item_id", Json::str(self.item_id.clone()))
            .with("min_price_cents", Json::int(self.min_price_cents))
            .with("max_price_cents", Json::int(self.max_price_cents))
            .with("step_cents", Json::int(self.step_cents))
            .with("anchor_price_cents", Json::int(self.anchor_price_cents))
            .with("floor_price_cents", Json::int(self.floor_price_cents))
            .with(
                "discounts",
                Json::Array(self.discounts.iter().map(|d| d.to_json()).collect()),
            )
            .with("version", Json::uint(self.version))
            .with("updated_at", Json::int(self.updated_at))
            .with("enabled", Json::Bool(self.enabled))
    }

    pub fn from_json(value: &Json) -> Result<PriceRule> {
        let rule = PriceRule {
            item_id: value
                .get_str("item_id")
                .ok_or_else(|| PricingError::new(codes::INVALID_RULE, "item_id is required"))?
                .to_string(),
            min_price_cents: value.get_i64("min_price_cents").ok_or_else(|| {
                PricingError::new(codes::INVALID_RULE, "min_price_cents is required")
            })?,
            max_price_cents: value.get_i64("max_price_cents").ok_or_else(|| {
                PricingError::new(codes::INVALID_RULE, "max_price_cents is required")
            })?,
            step_cents: value.get_i64("step_cents").unwrap_or(1),
            anchor_price_cents: value.get_i64("anchor_price_cents").ok_or_else(|| {
                PricingError::new(codes::INVALID_RULE, "anchor_price_cents is required")
            })?,
            floor_price_cents: value.get_i64("floor_price_cents").unwrap_or(1),
            discounts: value
                .get("discounts")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .map(DiscountRule::from_json)
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?
                .unwrap_or_default(),
            version: value.get("version").and_then(|v| v.as_u64()).unwrap_or(1),
            updated_at: value.get_i64("updated_at").unwrap_or(0),
            enabled: value
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        };
        rule.validate()?;
        Ok(rule)
    }
}

// --------------------------------------------------------------------------- price changes

/// Why a price changed (written into the audit log).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceChange {
    pub item_id: String,
    pub previous_cents: i64,
    /// Price after the random walk, before discounts.
    pub new_cents: i64,
    /// Price after active discounts (what a customer would pay now).
    pub discounted_cents: i64,
    pub rule_version: u64,
    pub at: i64,
    /// The raw random sample, recorded so the decision can be replayed.
    pub sample: u64,
    pub applied_discounts: Vec<String>,
    pub clamped: bool,
}

impl PriceChange {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("item_id", Json::str(self.item_id.clone()))
            .with("previous_cents", Json::int(self.previous_cents))
            .with("new_cents", Json::int(self.new_cents))
            .with("discounted_cents", Json::int(self.discounted_cents))
            .with("rule_version", Json::uint(self.rule_version))
            .with("at", Json::int(self.at))
            // Hex string: JSON numbers follow IEEE-754 in this codebase, so a 64-bit
            // sample would lose precision as a float.
            .with("sample", Json::str(format!("{:016x}", self.sample)))
            .with("clamped", Json::Bool(self.clamped))
            .with(
                "applied_discounts",
                Json::Array(
                    self.applied_discounts
                        .iter()
                        .map(|d| Json::str(d.clone()))
                        .collect(),
                ),
            )
    }
}

/// A price offer bound to a specific rule version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceOffer {
    pub offer_id: String,
    pub item_id: String,
    pub price_cents: i64,
    pub rule_version: u64,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub redeemed: bool,
}

impl PriceOffer {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("offer_id", Json::str(self.offer_id.clone()))
            .with("item_id", Json::str(self.item_id.clone()))
            .with("price_cents", Json::int(self.price_cents))
            .with("rule_version", Json::uint(self.rule_version))
            .with("created_at", Json::int(self.created_at))
            .with(
                "expires_at",
                self.expires_at.map(Json::int).unwrap_or(Json::Null),
            )
            .with("redeemed", Json::Bool(self.redeemed))
    }
}

/// A completed purchase. Deliberately has no setter: history is immutable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Purchase {
    pub purchase_id: String,
    pub item_id: String,
    pub offer_id: Option<String>,
    /// The amount that was actually charged, in cents.
    pub price_cents: i64,
    pub rule_version: u64,
    pub purchased_at: i64,
    pub quantity: u32,
}

impl Purchase {
    pub fn total_cents(&self) -> i64 {
        self.price_cents.saturating_mul(self.quantity as i64)
    }

    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("purchase_id", Json::str(self.purchase_id.clone()))
            .with("item_id", Json::str(self.item_id.clone()))
            .with(
                "offer_id",
                self.offer_id.clone().map(Json::str).unwrap_or(Json::Null),
            )
            .with("price_cents", Json::int(self.price_cents))
            .with("total_cents", Json::int(self.total_cents()))
            .with("rule_version", Json::uint(self.rule_version))
            .with("purchased_at", Json::int(self.purchased_at))
            .with("quantity", Json::uint(self.quantity as u64))
    }
}

/// Append-only purchase ledger. There is intentionally no update or delete method.
#[derive(Clone, Debug, Default)]
pub struct PurchaseLedger {
    purchases: Vec<Purchase>,
}

impl PurchaseLedger {
    pub fn record(&mut self, purchase: Purchase) -> Result<()> {
        if purchase.price_cents < 1 {
            return Err(PricingError::new(
                codes::OUT_OF_BOUNDS,
                "a purchase price must be at least 1 cent",
            ));
        }
        if purchase.purchase_id.trim().is_empty() {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                "purchase_id is required",
            ));
        }
        if self
            .purchases
            .iter()
            .any(|p| p.purchase_id == purchase.purchase_id)
        {
            return Err(PricingError::new(
                codes::DUPLICATE,
                format!("purchase '{}' already recorded", purchase.purchase_id),
            ));
        }
        self.purchases.push(purchase);
        Ok(())
    }

    pub fn get(&self, purchase_id: &str) -> Option<&Purchase> {
        self.purchases.iter().find(|p| p.purchase_id == purchase_id)
    }

    pub fn all(&self) -> &[Purchase] {
        &self.purchases
    }

    pub fn total_cents(&self) -> i64 {
        self.purchases.iter().map(|p| p.total_cents()).sum()
    }
}

// --------------------------------------------------------------------------- audit log

/// One entry of the hash-chained audit log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    pub seq: u64,
    pub at: i64,
    pub kind: String,
    pub detail: Json,
    /// SHA-256 of the previous entry's hash (hex); 64 zeros for the first entry.
    pub prev_hash: String,
    /// SHA-256 over `seq|at|kind|detail|prev_hash`.
    pub hash: String,
}

impl AuditEntry {
    fn compute_hash(seq: u64, at: i64, kind: &str, detail: &Json, prev_hash: &str) -> String {
        let material = format!("{seq}|{at}|{kind}|{}|{prev_hash}", detail);
        preppydb_crypto::hex_encode(&sha256(material.as_bytes()))
    }

    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("seq", Json::uint(self.seq))
            .with("at", Json::int(self.at))
            .with("kind", Json::str(self.kind.clone()))
            .with("detail", self.detail.clone())
            .with("prev_hash", Json::str(self.prev_hash.clone()))
            .with("hash", Json::str(self.hash.clone()))
    }

    pub fn from_json(value: &Json) -> Result<AuditEntry> {
        Ok(AuditEntry {
            seq: value.get("seq").and_then(|v| v.as_u64()).unwrap_or(0),
            at: value.get_i64("at").unwrap_or(0),
            kind: value.get_str("kind").unwrap_or("").to_string(),
            detail: value.get("detail").cloned().unwrap_or(Json::Null),
            prev_hash: value.get_str("prev_hash").unwrap_or("").to_string(),
            hash: value.get_str("hash").unwrap_or("").to_string(),
        })
    }
}

/// Append-only, tamper-evident audit log (one JSON object per line).
#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    entries: Vec<AuditEntry>,
}

impl AuditLog {
    /// Open (or create) an audit log file and load its existing entries.
    pub fn open(path: impl Into<PathBuf>) -> Result<AuditLog> {
        let path = path.into();
        let mut log = AuditLog {
            path,
            entries: Vec::new(),
        };
        if log.path.exists() {
            let text = std::fs::read_to_string(&log.path).map_err(|err| {
                PricingError::new(codes::AUDIT_INTEGRITY, format!("reading audit log: {err}"))
            })?;
            for (line_no, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let value = preppydb_json::parse(line).map_err(|err| {
                    PricingError::new(
                        codes::AUDIT_INTEGRITY,
                        format!("audit log line {} is not JSON: {err}", line_no + 1),
                    )
                })?;
                log.entries.push(AuditEntry::from_json(&value)?);
            }
            log.verify()?;
        }
        Ok(log)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }

    /// Append an entry, chaining it to the previous one and flushing to disk.
    pub fn append(&mut self, kind: &str, at: i64, detail: Json) -> Result<AuditEntry> {
        let seq = self.entries.len() as u64 + 1;
        let prev_hash = match self.entries.last() {
            Some(entry) => entry.hash.clone(),
            None => "0".repeat(64),
        };
        let hash = AuditEntry::compute_hash(seq, at, kind, &detail, &prev_hash);
        let entry = AuditEntry {
            seq,
            at,
            kind: kind.to_string(),
            detail,
            prev_hash,
            hash,
        };
        let line = format!("{}\n", entry.to_json());
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    PricingError::new(codes::AUDIT_INTEGRITY, format!("creating log dir: {err}"))
                })?;
            }
        }
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path).map_err(|err| {
            PricingError::new(codes::AUDIT_INTEGRITY, format!("opening audit log: {err}"))
        })?;
        file.write_all(line.as_bytes()).map_err(|err| {
            PricingError::new(codes::AUDIT_INTEGRITY, format!("writing audit log: {err}"))
        })?;
        file.sync_data().map_err(|err| {
            PricingError::new(codes::AUDIT_INTEGRITY, format!("syncing audit log: {err}"))
        })?;
        self.entries.push(entry.clone());
        Ok(entry)
    }

    /// Verify the hash chain (detects edits, reordering and truncation at the end).
    pub fn verify(&self) -> Result<()> {
        let mut previous = "0".repeat(64);
        for (index, entry) in self.entries.iter().enumerate() {
            let expected_seq = index as u64 + 1;
            if entry.seq != expected_seq {
                return Err(PricingError::new(
                    codes::AUDIT_INTEGRITY,
                    format!(
                        "audit entry {index} has seq {} (expected {expected_seq})",
                        entry.seq
                    ),
                ));
            }
            if entry.prev_hash != previous {
                return Err(PricingError::new(
                    codes::AUDIT_INTEGRITY,
                    format!("audit entry {index} does not chain to its predecessor"),
                ));
            }
            let recomputed = AuditEntry::compute_hash(
                entry.seq,
                entry.at,
                &entry.kind,
                &entry.detail,
                &entry.prev_hash,
            );
            if recomputed != entry.hash {
                return Err(PricingError::new(
                    codes::AUDIT_INTEGRITY,
                    format!("audit entry {index} hash mismatch (log was modified)"),
                ));
            }
            previous = entry.hash.clone();
        }
        Ok(())
    }
}

// --------------------------------------------------------------------------- engine

/// The pricing engine: deterministic random walk inside rule bounds.
pub struct PricingEngine {
    rng: ChaCha20Rng,
    seed_description: String,
}

impl PricingEngine {
    /// Create an engine with a fixed seed (reproducible runs).
    pub fn with_seed(seed: &[u8]) -> PricingEngine {
        PricingEngine {
            rng: ChaCha20Rng::from_seed(seed),
            seed_description: preppydb_crypto::hex_encode(&sha256(seed)),
        }
    }

    /// Create an engine from the OS entropy source (non-reproducible).
    pub fn from_entropy() -> Result<PricingEngine> {
        let rng = ChaCha20Rng::from_entropy()
            .map_err(|err| PricingError::new(codes::INVALID_RULE, format!("no entropy: {err}")))?;
        Ok(PricingEngine {
            rng,
            seed_description: "os-entropy".to_string(),
        })
    }

    /// SHA-256 of the seed, recorded in the audit log so a run can be reproduced.
    pub fn seed_fingerprint(&self) -> &str {
        &self.seed_description
    }

    /// Compute the next price for `current_cents`. The result is always inside the rule
    /// bounds, never negative, and never moves by more than `step_cents`.
    pub fn next_price(
        &mut self,
        rule: &PriceRule,
        current_cents: i64,
        now_ms: i64,
    ) -> Result<PriceChange> {
        rule.validate()?;
        if !rule.enabled {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                format!("rule for '{}' is disabled", rule.item_id),
            ));
        }
        let current = rule.clamp(current_cents);
        let sample = self.rng.next_u64();
        // Map the sample into [-step, +step].
        let step = rule.step_cents.max(0);
        let span = step * 2 + 1;
        let offset = if span <= 1 {
            0
        } else {
            (sample % span as u64) as i64 - step
        };
        let proposed = current + offset;
        let clamped_price = rule.clamp(proposed);
        let was_clamped = clamped_price != proposed;
        let (discounted, applied) = self.apply_discounts(rule, clamped_price, now_ms);
        Ok(PriceChange {
            item_id: rule.item_id.clone(),
            previous_cents: current,
            new_cents: clamped_price,
            discounted_cents: discounted,
            rule_version: rule.version,
            at: now_ms,
            sample,
            applied_discounts: applied,
            clamped: was_clamped,
        })
    }

    /// Apply all active discounts, never below the rule floor.
    pub fn apply_discounts(
        &self,
        rule: &PriceRule,
        price_cents: i64,
        now_ms: i64,
    ) -> (i64, Vec<String>) {
        let mut applied = Vec::new();
        let mut price = price_cents;
        for discount in &rule.discounts {
            if discount.is_active(now_ms) {
                let reduced = price - (price * discount.percent as i64 / 100);
                price = reduced.max(rule.floor_price_cents.max(1));
                applied.push(discount.label.clone());
            }
        }
        (price.max(1), applied)
    }

    /// Run `steps` price changes and return the history (used by tests and by
    /// `pricing simulate`).
    pub fn simulate(
        &mut self,
        rule: &mut PriceRule,
        steps: usize,
        start_ms: i64,
    ) -> Result<Vec<PriceChange>> {
        let mut history = Vec::with_capacity(steps);
        let mut current = rule.anchor_price_cents;
        for step in 0..steps {
            let now = start_ms + step as i64;
            let change = self.next_price(rule, current, now)?;
            current = change.new_cents;
            rule.version += 1;
            history.push(change);
        }
        Ok(history)
    }

    /// Create an offer at the current effective price.
    pub fn create_offer(
        &mut self,
        rule: &PriceRule,
        now_ms: i64,
        ttl_ms: Option<i64>,
    ) -> Result<PriceOffer> {
        rule.validate()?;
        let (discounted, _) = self.apply_discounts(rule, rule.anchor_price_cents, now_ms);
        let mut id_bytes = [0u8; 16];
        self.rng.fill_bytes(&mut id_bytes);
        Ok(PriceOffer {
            offer_id: format!("offer-{}", preppydb_crypto::base64_url_encode(&id_bytes)),
            item_id: rule.item_id.clone(),
            price_cents: discounted.max(1),
            rule_version: rule.version,
            created_at: now_ms,
            expires_at: ttl_ms.map(|ttl| now_ms + ttl),
            redeemed: false,
        })
    }

    /// Redeem an offer, returning the purchase to be recorded.
    ///
    /// Fails when the offer is unknown, already redeemed, expired, or was created for a
    /// different (older) version of the rule.
    pub fn redeem_offer(
        &mut self,
        rule: &PriceRule,
        offer: &PriceOffer,
        quantity: u32,
        now_ms: i64,
    ) -> Result<Purchase> {
        if offer.redeemed {
            return Err(PricingError::new(
                codes::OFFER_REDEEMED,
                format!("offer '{}' has already been redeemed", offer.offer_id),
            ));
        }
        if offer.rule_version != rule.version {
            return Err(PricingError::new(
                codes::OFFER_STALE,
                format!(
                    "offer '{}' was created for rule version {} but the rule is now version {}",
                    offer.offer_id, offer.rule_version, rule.version
                ),
            ));
        }
        if let Some(expires_at) = offer.expires_at {
            if now_ms > expires_at {
                return Err(PricingError::new(
                    codes::OFFER_EXPIRED,
                    format!("offer '{}' expired at {expires_at}", offer.offer_id),
                ));
            }
        }
        if quantity == 0 {
            return Err(PricingError::new(
                codes::INVALID_RULE,
                "quantity must be at least 1",
            ));
        }
        let price = offer.price_cents;
        if price < 1 || price > rule.max_price_cents.max(price) {
            return Err(PricingError::new(
                codes::OUT_OF_BOUNDS,
                format!("offer price {price} is outside the rule bounds"),
            ));
        }
        let mut id_bytes = [0u8; 16];
        self.rng.fill_bytes(&mut id_bytes);
        Ok(Purchase {
            purchase_id: format!("purchase-{}", preppydb_crypto::base64_url_encode(&id_bytes)),
            item_id: offer.item_id.clone(),
            offer_id: Some(offer.offer_id.clone()),
            price_cents: price,
            rule_version: offer.rule_version,
            purchased_at: now_ms,
            quantity,
        })
    }
}

/// Convenience: catalogue snapshot `{item_id: effective_price}` for a set of rules.
pub fn effective_prices(rules: &[PriceRule], now_ms: i64) -> BTreeMap<String, i64> {
    let engine = PricingEngine::with_seed(b"price-catalogue");
    let mut prices = BTreeMap::new();
    for rule in rules {
        let (price, _) = engine.apply_discounts(rule, rule.anchor_price_cents, now_ms);
        prices.insert(rule.item_id.clone(), price.max(1));
    }
    prices
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> PriceRule {
        PriceRule::new("hat", 100, 500, 300)
    }

    #[test]
    fn validation_rejects_unsafe_rules() {
        let mut bad = rule();
        bad.min_price_cents = 0;
        assert!(bad.validate().is_err(), "zero minimum must be rejected");
        bad = rule();
        bad.min_price_cents = 500;
        bad.max_price_cents = 100;
        assert!(bad.validate().is_err());
        bad = rule();
        bad.anchor_price_cents = 900;
        assert!(bad.validate().is_err());
        bad = rule();
        bad.step_cents = -1;
        assert!(bad.validate().is_err());
        bad = rule();
        bad.discounts = vec![DiscountRule {
            label: "too much".into(),
            percent: 99,
            starts_at: None,
            ends_at: None,
        }];
        assert!(bad.validate().is_err());
        assert!(rule().validate().is_ok());
    }

    #[test]
    fn prices_stay_inside_bounds() {
        let mut engine = PricingEngine::with_seed(b"bounds");
        let mut rule = PriceRule::new("item", 100, 200, 150);
        rule.step_cents = 1000; // step larger than the interval: clamping must kick in
        let mut price = 150;
        for step in 0..1000 {
            let change = engine.next_price(&rule, price, step).unwrap();
            assert!(
                change.new_cents >= 100 && change.new_cents <= 200,
                "{change:?}"
            );
            assert!(change.discounted_cents >= 1);
            price = change.new_cents;
        }
    }

    #[test]
    fn step_limits_movement() {
        let mut engine = PricingEngine::with_seed(b"step");
        let rule = PriceRule {
            step_cents: 5,
            ..PriceRule::new("i", 100, 10_000, 5000)
        };
        let mut price = 5000;
        for step in 0..500 {
            let change = engine.next_price(&rule, price, step).unwrap();
            assert!((change.new_cents - price).abs() <= 5, "{change:?}");
            price = change.new_cents;
        }
    }

    #[test]
    fn same_seed_same_history() {
        let mut a = PricingEngine::with_seed(b"seed-42");
        let mut b = PricingEngine::with_seed(b"seed-42");
        let mut c = PricingEngine::with_seed(b"seed-43");
        let mut rule_a = rule();
        let mut rule_b = rule();
        let mut rule_c = rule();
        let history_a = a.simulate(&mut rule_a, 50, 0).unwrap();
        let history_b = b.simulate(&mut rule_b, 50, 0).unwrap();
        let history_c = c.simulate(&mut rule_c, 50, 0).unwrap();
        assert_eq!(history_a, history_b);
        assert_ne!(history_a, history_c);
    }

    #[test]
    fn discounts_apply_and_never_go_negative() {
        let engine = PricingEngine::with_seed(b"discounts");
        let mut rule = PriceRule::new("item", 10, 1000, 100);
        rule.discounts = vec![
            DiscountRule {
                label: "spring".into(),
                percent: 50,
                starts_at: None,
                ends_at: None,
            },
            DiscountRule {
                label: "clearance".into(),
                percent: 90,
                starts_at: None,
                ends_at: None,
            },
        ];
        let (price, applied) = engine.apply_discounts(&rule, 100, 0);
        assert_eq!(applied, vec!["spring".to_string(), "clearance".to_string()]);
        // 100 -> 50 (spring) -> 5 (clearance); the floor only applies below 1 cent.
        assert_eq!(price, 5);
        // Even a pathological 90% discount chain on 1 cent stays positive.
        let (tiny, _) = engine.apply_discounts(&rule, 1, 0);
        assert_eq!(tiny, 1);

        // Expired / future discounts do not apply.
        rule.discounts = vec![DiscountRule {
            label: "past".into(),
            percent: 50,
            starts_at: Some(0),
            ends_at: Some(10),
        }];
        let (price, applied) = engine.apply_discounts(&rule, 100, 100);
        assert!(applied.is_empty());
        assert_eq!(price, 100);
    }

    #[test]
    fn offers_are_version_bound() {
        let mut engine = PricingEngine::with_seed(b"offers");
        let rule = rule();
        let offer = engine.create_offer(&rule, 1000, Some(10_000)).unwrap();
        assert_eq!(offer.rule_version, rule.version);
        // Valid redemption.
        let mut changed_rule = rule.clone();
        let purchase = engine.redeem_offer(&changed_rule, &offer, 2, 2000).unwrap();
        assert_eq!(purchase.price_cents, offer.price_cents);
        assert_eq!(purchase.total_cents(), offer.price_cents * 2);
        // A rule change invalidates the offer.
        changed_rule.version += 1;
        let err = engine
            .redeem_offer(&changed_rule, &offer, 1, 3000)
            .unwrap_err();
        assert_eq!(err.code, codes::OFFER_STALE);
        // Expiry.
        let err = engine.redeem_offer(&rule, &offer, 1, 20_000).unwrap_err();
        assert_eq!(err.code, codes::OFFER_EXPIRED);
        // Double redemption.
        let mut redeemed = offer.clone();
        redeemed.redeemed = true;
        let err = engine.redeem_offer(&rule, &redeemed, 1, 2000).unwrap_err();
        assert_eq!(err.code, codes::OFFER_REDEEMED);
    }

    #[test]
    fn purchases_are_immutable_history() {
        let mut ledger = PurchaseLedger::default();
        let mut engine = PricingEngine::with_seed(b"ledger");
        let rule = rule();
        let offer = engine.create_offer(&rule, 0, None).unwrap();
        let purchase = engine.redeem_offer(&rule, &offer, 1, 10).unwrap();
        let charged = purchase.price_cents;
        ledger.record(purchase.clone()).unwrap();
        assert_eq!(
            ledger.get(&purchase.purchase_id).unwrap().price_cents,
            charged
        );
        // Re-recording the same purchase id is rejected.
        assert!(ledger.record(purchase.clone()).is_err());
        // A later, cheaper rule does not rewrite the recorded amount.
        let mut later_rule = rule.clone();
        later_rule.version += 5;
        later_rule.anchor_price_cents = later_rule.min_price_cents;
        let _ = later_rule;
        assert_eq!(
            ledger.get(&purchase.purchase_id).unwrap().price_cents,
            charged
        );
        assert_eq!(ledger.total_cents(), charged);
    }

    #[test]
    fn audit_log_chain_detects_tampering() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("audit.log");
        let mut log = AuditLog::open(&path).unwrap();
        log.append(
            "price_change",
            100,
            Json::obj().with("item", Json::str("hat")),
        )
        .unwrap();
        log.append(
            "offer_created",
            200,
            Json::obj().with("item", Json::str("hat")),
        )
        .unwrap();
        log.append("purchase", 300, Json::obj().with("amount", Json::int(250)))
            .unwrap();
        log.verify().unwrap();
        assert_eq!(log.entries().len(), 3);
        assert_eq!(log.entries()[1].prev_hash, log.entries()[0].hash);

        // Reload from disk: the chain must still verify.
        let reloaded = AuditLog::open(&path).unwrap();
        assert_eq!(reloaded.entries().len(), 3);

        // Tamper with one line and expect verification to fail.
        let text = std::fs::read_to_string(&path).unwrap();
        let tampered = text.replacen("\"amount\":250", "\"amount\":1", 1);
        assert_ne!(text, tampered);
        std::fs::write(&path, tampered).unwrap();
        let err = AuditLog::open(&path).unwrap_err();
        assert_eq!(err.code, codes::AUDIT_INTEGRITY);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rule_json_roundtrip_and_validation() {
        let mut rule = rule();
        rule.discounts.push(DiscountRule {
            label: "launch".into(),
            percent: 20,
            starts_at: Some(1),
            ends_at: Some(2),
        });
        let encoded = rule.to_json();
        let decoded = PriceRule::from_json(&encoded).unwrap();
        assert_eq!(rule, decoded);
        let bad = preppydb_json::parse(
            r#"{"item_id":"x","min_price_cents":-5,"max_price_cents":10,"anchor_price_cents":5}"#,
        )
        .unwrap();
        assert!(PriceRule::from_json(&bad).is_err());
    }

    #[test]
    fn effective_prices_are_deterministic() {
        let rules = vec![
            PriceRule::new("a", 100, 200, 150),
            PriceRule::new("b", 50, 80, 60),
        ];
        let first = effective_prices(&rules, 0);
        let second = effective_prices(&rules, 0);
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
    }
}
