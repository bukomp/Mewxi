//! Model pricing with dynamic refresh from LiteLLM's public price table.
//!
//! On first lookup we try to load `~/.cache/mewxi/litellm_prices.json`; if
//! it's missing or older than 24h we fetch the upstream JSON (one-shot,
//! short timeout) and rewrite the cache. Any failure silently falls back
//! to the hard-coded [`fallback`] rates so mewxi keeps working offline.
//!
//! Lookup is by model *family* (`opus`/`sonnet`/`haiku`), matching how
//! Claude Code reports the model id. We pick the bare
//! `anthropic.claude-<family>-…` entry (no region prefix) so we don't
//! accidentally use AWS Bedrock surcharge pricing.
//!
//! Values are normalized to **USD per million tokens** so they can be
//! used the same way the hard-coded table was.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

#[derive(Default, Clone, Debug)]
struct Table {
    opus: Option<ModelPrice>,
    sonnet: Option<ModelPrice>,
    haiku: Option<ModelPrice>,
}

pub fn price_for(model: &str) -> ModelPrice {
    let t = table();
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        t.opus.unwrap_or_else(|| fallback("opus"))
    } else if m.contains("haiku") {
        t.haiku.unwrap_or_else(|| fallback("haiku"))
    } else {
        t.sonnet.unwrap_or_else(|| fallback("sonnet"))
    }
}

fn table() -> &'static Table {
    static T: OnceLock<Table> = OnceLock::new();
    T.get_or_init(load_table)
}

fn load_table() -> Table {
    let raw = load_or_fetch_raw();
    raw.as_deref().and_then(parse_table).unwrap_or_default()
}

fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|c| c.join("mewxi").join("litellm_prices.json"))
}

fn load_or_fetch_raw() -> Option<String> {
    if let Some(p) = cache_path() {
        if let Ok(meta) = fs::metadata(&p) {
            if let Ok(modified) = meta.modified() {
                if SystemTime::now().duration_since(modified).unwrap_or(CACHE_TTL) < CACHE_TTL {
                    if let Ok(s) = fs::read_to_string(&p) {
                        return Some(s);
                    }
                }
            }
        }
    }
    let body = fetch_remote()?;
    if let Some(p) = cache_path() {
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&p, &body);
    }
    Some(body)
}

fn fetch_remote() -> Option<String> {
    ureq::get(LITELLM_URL)
        .timeout(FETCH_TIMEOUT)
        .call()
        .ok()?
        .into_string()
        .ok()
}

fn parse_table(raw: &str) -> Option<Table> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let obj = v.as_object()?;
    let mut t = Table::default();
    for family in ["opus", "sonnet", "haiku"] {
        let key = best_key_for(obj, family)?;
        if let Some(price) = extract_price(&obj[&key]) {
            match family {
                "opus" => t.opus = Some(price),
                "sonnet" => t.sonnet = Some(price),
                "haiku" => t.haiku = Some(price),
                _ => {}
            }
        }
    }
    Some(t)
}

// Pick the bare `anthropic.claude-<family>-…` (or `claude-<family>-…`)
// entry with the highest version. We avoid region-prefixed Bedrock keys
// (`us.`, `eu.`, `au.`) which carry surcharge pricing.
fn best_key_for(obj: &serde_json::Map<String, Value>, family: &str) -> Option<String> {
    let needle = format!("claude-{family}");
    let mut best: Option<String> = None;
    for key in obj.keys() {
        let k = key.to_ascii_lowercase();
        if !k.contains(&needle) {
            continue;
        }
        if k.starts_with("us.") || k.starts_with("eu.") || k.starts_with("au.") || k.starts_with("apac.") {
            continue;
        }
        if !(k.starts_with("anthropic.") || k.starts_with("claude-")) {
            continue;
        }
        // Prefer the lexicographically largest key, which for these
        // names sorts the highest version last.
        match &best {
            Some(b) if b >= &k => {}
            _ => best = Some(key.clone()),
        }
    }
    best
}

fn extract_price(entry: &Value) -> Option<ModelPrice> {
    let input = num(entry, "input_cost_per_token")?;
    let output = num(entry, "output_cost_per_token")?;
    let cache_read = num(entry, "cache_read_input_token_cost").unwrap_or(input * 0.1);
    let cache_write_5m =
        num(entry, "cache_creation_input_token_cost").unwrap_or(input * 1.25);
    let cache_write_1h =
        num(entry, "cache_creation_input_token_cost_above_1hr").unwrap_or(input * 2.0);
    Some(ModelPrice {
        input: input * 1e6,
        output: output * 1e6,
        cache_write_5m: cache_write_5m * 1e6,
        cache_write_1h: cache_write_1h * 1e6,
        cache_read: cache_read * 1e6,
    })
}

fn num(entry: &Value, field: &str) -> Option<f64> {
    entry.get(field)?.as_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_to_end_fetch_and_lookup() {
        // Clears the on-disk cache so this exercises the real fetch path.
        if let Some(p) = cache_path() {
            let _ = fs::remove_file(&p);
        }
        let opus = price_for("claude-opus-4-7");
        let sonnet = price_for("claude-sonnet-4-6");
        let haiku = price_for("claude-haiku-4-5");
        // Sanity ranges so a wildly wrong parse fails CI.
        assert!(opus.input > 1.0 && opus.input < 20.0, "opus.input={}", opus.input);
        assert!(sonnet.input > 1.0 && sonnet.input < 10.0, "sonnet.input={}", sonnet.input);
        assert!(haiku.input > 0.1 && haiku.input < 5.0, "haiku.input={}", haiku.input);
        eprintln!("opus={:?} sonnet={:?} haiku={:?}", opus, sonnet, haiku);
    }
}

fn fallback(family: &str) -> ModelPrice {
    match family {
        "opus" => ModelPrice {
            input: 5.0,
            output: 25.0,
            cache_write_5m: 6.25,
            cache_write_1h: 10.0,
            cache_read: 0.5,
        },
        "haiku" => ModelPrice {
            input: 1.0,
            output: 5.0,
            cache_write_5m: 1.25,
            cache_write_1h: 2.0,
            cache_read: 0.1,
        },
        _ => ModelPrice {
            input: 3.0,
            output: 15.0,
            cache_write_5m: 3.75,
            cache_write_1h: 6.0,
            cache_read: 0.3,
        },
    }
}
