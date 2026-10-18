//! List API prices for the models Godspeed's harnesses run, shared by the
//! transcript receipts (UI) and the usage history (engine).
//!
//! Every figure here is an estimate of what tokens would cost billed per
//! token on the provider's API. Subscription logins pay nothing per token;
//! the usage page still shows the figure, labelled "at API prices", the way
//! T3 Code does ("if billed at full API rate").

/// When the table below was last checked against published prices.
pub const PRICES_CHECKED: &str = "2026-10-18";

/// List API prices in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    /// 5-minute cache writes (1.25x input on Anthropic models). OpenAI bills
    /// no cache writes (Codex never reports any); its rows repeat `input`.
    pub cache_write: f64,
}

impl Price {
    /// USD for these token counts. `input` excludes cache traffic, which
    /// rides its own counts.
    pub fn cost(&self, input: u64, output: u64, cache_read: u64, cache_write: u64) -> f64 {
        let per = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
        per(input, self.input)
            + per(output, self.output)
            + per(cache_read, self.cache_read)
            + per(cache_write, self.cache_write)
    }

    /// What reading `cache_read` tokens from cache saved over sending them
    /// as plain input.
    pub fn cache_savings(&self, cache_read: u64) -> f64 {
        cache_read as f64 * (self.input - self.cache_read).max(0.0) / 1_000_000.0
    }
}

const fn p(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Price {
    Price {
        input,
        output,
        cache_read,
        cache_write,
    }
}

/// Local price table. Current Claude rows are Anthropic's first-party list
/// prices (claude-api reference cached 2026-10-06); retired Claude models
/// carry their published launch prices with the standard cache ratios (0.1x
/// read, 1.25x write). OpenAI rows are OpenAI's published launch prices for
/// each model (`gpt-6-astra` from third-party listings that agreed with each
/// other; openai.com/api/pricing refused the fetch); the GPT-5.6 family is
/// left out because sources disagreed after the September price cut. Fast
/// mode, long-context and >100K-prompt tiers, batch and priority are not
/// modelled, so every figure is an estimate. A model missing here shows its
/// tokens with no cost rather than a guess — add rows as prices are confirmed.
const PRICES: &[(&str, Price)] = &[
    // Anthropic, current.
    ("claude-fable-5-1", p(10.0, 50.0, 0.25, 12.5)),
    ("claude-fable-5", p(10.0, 50.0, 1.0, 12.5)),
    ("claude-opus-5-5", p(4.0, 20.0, 0.2, 5.0)),
    ("claude-opus-5", p(5.0, 25.0, 0.5, 6.25)),
    ("claude-opus-4-8", p(5.0, 25.0, 0.5, 6.25)),
    ("claude-opus-4-7", p(5.0, 25.0, 0.5, 6.25)),
    ("claude-opus-4-6", p(5.0, 25.0, 0.5, 6.25)),
    ("claude-sonnet-5-5", p(2.0, 10.0, 0.2, 2.5)),
    ("claude-sonnet-5", p(2.0, 10.0, 0.2, 2.5)),
    ("claude-sonnet-4-6", p(3.0, 15.0, 0.3, 3.75)),
    ("claude-haiku-5-5", p(0.1, 0.5, 0.01, 0.125)),
    ("claude-haiku-4-5", p(1.0, 5.0, 0.1, 1.25)),
    // Anthropic, older (still in many transcripts).
    ("claude-opus-4-5", p(5.0, 25.0, 0.5, 6.25)),
    ("claude-opus-4-1", p(15.0, 75.0, 1.5, 18.75)),
    ("claude-opus-4", p(15.0, 75.0, 1.5, 18.75)),
    ("claude-sonnet-4-5", p(3.0, 15.0, 0.3, 3.75)),
    ("claude-sonnet-4", p(3.0, 15.0, 0.3, 3.75)),
    ("claude-3-7-sonnet", p(3.0, 15.0, 0.3, 3.75)),
    ("claude-3-5-haiku", p(0.8, 4.0, 0.08, 1.0)),
    ("claude-3-haiku", p(0.25, 1.25, 0.03, 0.3)),
    // OpenAI.
    ("gpt-6-astra", p(10.0, 50.0, 1.0, 12.5)),
    ("gpt-5.2", p(1.75, 14.0, 0.175, 1.75)),
    ("gpt-5.1-codex-max", p(1.25, 10.0, 0.125, 1.25)),
    ("gpt-5.1-codex-mini", p(0.25, 2.0, 0.025, 0.25)),
    ("gpt-5.1-codex", p(1.25, 10.0, 0.125, 1.25)),
    ("gpt-5.1", p(1.25, 10.0, 0.125, 1.25)),
    ("gpt-5-codex", p(1.25, 10.0, 0.125, 1.25)),
    ("gpt-5-mini", p(0.25, 2.0, 0.025, 0.25)),
    ("gpt-5-nano", p(0.05, 0.4, 0.005, 0.05)),
    ("gpt-5", p(1.25, 10.0, 0.125, 1.25)),
];

/// The id a model prices and groups under, without the decorations
/// harnesses add: a provider prefix (`anthropic/`, `us.anthropic.`), a
/// context tag (`[1m]`), a Vertex `@version` and a dated snapshot suffix
/// (`-20261001`).
pub fn canonical_model(model: &str) -> String {
    let mut id = model.trim().to_ascii_lowercase();
    if let Some(ix) = id.find(['[', '@']) {
        id.truncate(ix);
    }
    if let Some(ix) = id.rfind("claude-") {
        id.drain(..ix);
    } else if let Some(ix) = id.rfind('/') {
        id.drain(..=ix);
    }
    if let Some((base, date)) = id.rsplit_once('-')
        && date.len() == 8
        && date.bytes().all(|b| b.is_ascii_digit())
    {
        id = base.to_owned();
    }
    id
}

/// The table row for a model id ([`canonical_model`] first). Exact id
/// otherwise — `claude-opus-5` must never price `claude-opus-5-5`.
pub fn price_for(model: &str) -> Option<Price> {
    let id = canonical_model(model);
    PRICES
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, price)| *price)
}

/// Estimated USD for these token counts on `model`, or `None` for a model
/// the table doesn't price.
pub fn estimate_cost(
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
) -> Option<f64> {
    Some(price_for(model)?.cost(input, output, cache_read, cache_write))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_normalize_but_never_collide() {
        assert_eq!(
            price_for("claude-opus-5-5[1m]"),
            price_for("claude-opus-5-5")
        );
        assert_eq!(
            price_for("us.anthropic.claude-opus-4-8"),
            price_for("claude-opus-4-8")
        );
        assert_eq!(
            price_for("claude-haiku-4-5-20251001"),
            price_for("claude-haiku-4-5")
        );
        assert_eq!(
            price_for("claude-opus-4-5@20251101"),
            price_for("claude-opus-4-5")
        );
        assert_eq!(price_for("openai/gpt-5-codex"), price_for("gpt-5-codex"));
        assert_eq!(price_for("claude-3-5-haiku-20241022").unwrap().input, 0.8);
        assert_ne!(price_for("claude-opus-5-5"), price_for("claude-opus-5"));
        assert_ne!(price_for("gpt-5.1-codex-mini"), price_for("gpt-5.1-codex"));
        assert_eq!(price_for("gpt-5.6-sol"), None);
        assert_eq!(price_for("opus"), None);
        assert_eq!(price_for("<synthetic>"), None);
    }

    #[test]
    fn cost_counts_every_token_kind_at_its_own_rate() {
        // $4 + $2 + $0.40 + $0.50.
        let cost =
            estimate_cost("claude-opus-5-5", 1_000_000, 100_000, 2_000_000, 100_000).unwrap();
        assert!((cost - 6.9).abs() < 1e-9, "{cost}");
        let saved = price_for("claude-opus-5-5")
            .unwrap()
            .cache_savings(2_000_000);
        assert!((saved - 7.6).abs() < 1e-9, "{saved}");
        assert_eq!(estimate_cost("mystery-model", 1, 1, 1, 1), None);
    }
}
