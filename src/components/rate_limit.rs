//! Rate limit component implementation.
//!
//! Displays Claude.ai subscription rate-limit windows from the official
//! Claude Code stdin payload. When stdin carries no `rate_limits` (token auth
//! through a non-official gateway like cc-bridge, where Claude Code gates the
//! field behind claude.ai OAuth), it **auto-probes** `${ANTHROPIC_BASE_URL}
//! /v1/usage` — shown when the gateway answers, hidden otherwise. The probe is
//! throttled to at most once per minute per base URL via an on-disk cache, so
//! it never slows the per-render path.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::base::{Component, ComponentFactory, ComponentOutput, RenderContext};
use crate::config::{BaseComponentConfig, Config, RateLimitComponentConfig};
use crate::core::input::{RateLimitWindow, RateLimitsInfo};
use crate::utils;
use async_trait::async_trait;
use serde_json::Value;

/// Default usage endpoint path auto-probed on non-official gateways.
const DEFAULT_USAGE_PATH: &str = "/v1/usage";

/// Minute-level throttle: at most one usage probe per base URL per this window,
/// regardless of how often the statusline re-renders.
const USAGE_CACHE_TTL: Duration = Duration::from_secs(60);

/// Rate limit component.
pub struct RateLimitComponent {
    config: RateLimitComponentConfig,
}

impl RateLimitComponent {
    #[must_use]
    pub const fn new(config: RateLimitComponentConfig) -> Self {
        Self { config }
    }

    fn render_window(
        &self,
        label: &str,
        window: &RateLimitWindow,
        now_secs: i64,
    ) -> Option<String> {
        let mut parts = Vec::new();

        if let Some(used) = window.used_percentage {
            parts.push(format!("{label} {used:.0}%"));
        } else if window.resets_at.is_some() {
            parts.push(label.to_string());
        }

        if self.config.show_reset {
            if let Some(resets_at) = window.resets_at {
                parts.push(format!(
                    "reset {}",
                    Self::format_reset_duration(resets_at, now_secs)
                ));
            }
        }

        (!parts.is_empty()).then(|| parts.join(" "))
    }

    fn format_reset_duration(resets_at: i64, now_secs: i64) -> String {
        let remaining = resets_at.saturating_sub(now_secs);
        if remaining == 0 {
            return "now".to_string();
        }

        let days = remaining / 86_400;
        let hours = (remaining % 86_400) / 3_600;
        let minutes = ((remaining % 3_600) / 60).max(1);

        if days > 0 {
            if hours > 0 {
                format!("{days}d{hours}h")
            } else {
                format!("{days}d")
            }
        } else if hours > 0 {
            format!("{hours}h{minutes}m")
        } else {
            format!("{minutes}m")
        }
    }

    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            })
    }

    /// Render the configured windows of a `RateLimitsInfo` into display strings.
    /// Shared by the stdin path and the usage-endpoint fallback.
    fn build_windows(&self, rate_limits: &RateLimitsInfo, now_secs: i64) -> Vec<String> {
        let mut windows = Vec::new();

        if self.config.show_five_hour {
            if let Some(window) = rate_limits.five_hour.as_ref() {
                if let Some(rendered) = self.render_window("5h", window, now_secs) {
                    windows.push(rendered);
                }
            }
        }

        if self.config.show_seven_day {
            if let Some(window) = rate_limits.seven_day.as_ref() {
                if let Some(rendered) = self.render_window("7d", window, now_secs) {
                    windows.push(rendered);
                }
            }
        }

        windows
    }

    /// Auto-probe the usage endpoint when stdin gave us no limits and we're
    /// talking to a non-official gateway. Adaptive: shows when the endpoint
    /// answers with usage, hides otherwise. Throttled to ~1 request/min per base
    /// URL via an mtime-based cache so the render path stays cheap.
    async fn fetch_usage_windows(&self) -> Option<RateLimitsInfo> {
        // Endpoint path: config override, else auto default. Empty = disabled.
        let path = self
            .config
            .usage_endpoint
            .as_deref()
            .unwrap_or(DEFAULT_USAGE_PATH);
        if path.is_empty() {
            return None;
        }

        // Only probe non-official gateways; the official endpoint delivers
        // limits via stdin and has no /v1/usage.
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|base| !base.trim().is_empty())?;
        if Self::is_official_base(&base_url) {
            return None;
        }

        // Optional extra gate: base URL must contain this substring.
        if let Some(needle) = self.config.usage_endpoint_detect.as_deref() {
            if !base_url.contains(needle) {
                return None;
            }
        }

        let url = Self::build_usage_url(path, Some(&base_url))?;
        let cache_path = Self::usage_cache_path(&base_url);

        // Fresh cache → use verbatim. An empty payload means "probed recently,
        // nothing to show" and suppresses a re-probe until the cache expires.
        if let Some(cache_path) = cache_path.as_ref() {
            if let Some(cached) = Self::read_fresh_cache(cache_path, USAGE_CACHE_TTL) {
                return Self::usage_from_json(&cached);
            }
        }

        // Stale/absent cache → probe once, then persist the result (or `{}`).
        let token = std::env::var("ANTHROPIC_AUTH_TOKEN")
            .ok()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok());
        let timeout = Duration::from_millis(self.config.usage_timeout_ms);
        let insecure = Self::tls_skip_enabled(&url);

        let body = tokio::task::spawn_blocking(move || {
            Self::http_get(&url, token.as_deref(), timeout, insecure)
        })
        .await
        .ok()
        .flatten();

        let json = body.and_then(|body| serde_json::from_str::<Value>(&body).ok());

        // Persist the payload — or an empty marker on failure — so the next
        // probe is at least `USAGE_CACHE_TTL` away (bounds re-probing).
        if let Some(cache_path) = cache_path.as_ref() {
            let to_cache = json
                .clone()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            Self::write_cache(cache_path, &to_cache);
        }

        json.as_ref().and_then(Self::usage_from_json)
    }

    /// The official Anthropic endpoint (limits arrive via stdin, no /v1/usage).
    fn is_official_base(base_url: &str) -> bool {
        base_url.contains("api.anthropic.com")
    }

    /// Blocking HTTP GET (run inside `spawn_blocking`). Honors an insecure TLS
    /// agent for self-signed gateways. Returns the body on 2xx, else `None`.
    fn http_get(
        url: &str,
        token: Option<&str>,
        timeout: Duration,
        insecure: bool,
    ) -> Option<String> {
        let mut request = if insecure {
            Self::insecure_agent().map_or_else(|| ureq::get(url), |agent| agent.get(url))
        } else {
            ureq::get(url)
        };
        request = request.timeout(timeout);
        if let Some(token) = token {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        request = request.set("User-Agent", "claude-code-statusline/3.0");

        match request.call() {
            Ok(response) => response.into_string().ok(),
            Err(err) => {
                eprintln!("[statusline] rate_limit usage probe failed: {err}");
                None
            }
        }
    }

    /// Build a ureq agent that skips TLS verification, for self-signed gateways
    /// when the user set `NODE_TLS_REJECT_UNAUTHORIZED=0` (same knob Node uses).
    /// Uses the ring provider shared with ureq. Returns `None` on config error.
    fn insecure_agent() -> Option<ureq::Agent> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .ok()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertVerify))
            .with_no_client_auth();
        Some(
            ureq::AgentBuilder::new()
                .tls_config(Arc::new(config))
                .build(),
        )
    }

    /// Whether the probe should skip TLS verification for `url`.
    fn tls_skip_enabled(url: &str) -> bool {
        url.starts_with("https://")
            && std::env::var("NODE_TLS_REJECT_UNAUTHORIZED").is_ok_and(|value| value.trim() == "0")
    }

    /// Cache file path for a base URL (hashed to avoid path injection).
    fn usage_cache_path(base_url: &str) -> Option<PathBuf> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        base_url.hash(&mut hasher);
        let hash = hasher.finish();
        Some(
            utils::home_dir()?
                .join(".claude")
                .join("statusline-pro")
                .join("cache")
                .join(format!("rate-usage-{hash:016x}.json")),
        )
    }

    /// Read a cache file only if its mtime is within `ttl`; else `None`.
    fn read_fresh_cache(path: &Path, ttl: Duration) -> Option<Value> {
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;
        // Treat clock skew (now < modified) as stale to be safe.
        if SystemTime::now().duration_since(modified).unwrap_or(ttl) >= ttl {
            return None;
        }
        let contents = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    /// Atomically write a JSON value to the cache (tmp + rename). Best-effort.
    fn write_cache(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(serialized) = serde_json::to_string(value) else {
            return;
        };
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, serialized).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// Parse a usage payload into limits, or `None` when there is nothing to
    /// show (empty object / no windows).
    fn usage_from_json(json: &Value) -> Option<RateLimitsInfo> {
        let info = Self::parse_usage_json(json);
        if info.five_hour.is_none() && info.seven_day.is_none() {
            None
        } else {
            Some(info)
        }
    }

    /// Build the absolute request URL from a possibly-relative endpoint.
    fn build_usage_url(endpoint: &str, base_url: Option<&str>) -> Option<String> {
        if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            return Some(endpoint.to_string());
        }
        let base = base_url?;
        Some(format!("{}{}", base.trim_end_matches('/'), endpoint))
    }

    /// Parse a cc-bridge `/v1/usage` payload into `RateLimitsInfo`.
    ///
    /// Shape: `{"five_hour":{"utilization":6.0,"resets_at":"<rfc3339>"}, ...}`
    /// where `utilization` is already a 0-100 percentage.
    fn parse_usage_json(json: &Value) -> RateLimitsInfo {
        RateLimitsInfo {
            five_hour: Self::parse_usage_window(json.get("five_hour")),
            seven_day: Self::parse_usage_window(json.get("seven_day")),
        }
    }

    fn parse_usage_window(value: Option<&Value>) -> Option<RateLimitWindow> {
        let value = value?;
        let used_percentage = value.get("utilization").and_then(Value::as_f64);
        let resets_at = value
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(Self::parse_rfc3339_secs);

        if used_percentage.is_none() && resets_at.is_none() {
            return None;
        }

        Some(RateLimitWindow {
            used_percentage,
            resets_at,
        })
    }

    fn parse_rfc3339_secs(value: &str) -> Option<i64> {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|dt| dt.timestamp())
    }
}

#[async_trait]
impl Component for RateLimitComponent {
    fn name(&self) -> &'static str {
        "rate_limit"
    }

    fn is_enabled(&self, _ctx: &RenderContext) -> bool {
        self.config.base.enabled
    }

    async fn render(&self, ctx: &RenderContext) -> ComponentOutput {
        if !self.is_enabled(ctx) {
            return ComponentOutput::hidden();
        }

        let now_secs = Self::now_secs();

        // Primary source: official stdin `rate_limits` (claude.ai OAuth).
        let mut windows = ctx
            .input
            .rate_limits
            .as_ref()
            .map(|rate_limits| self.build_windows(rate_limits, now_secs))
            .unwrap_or_default();

        // Adaptive fallback: auto-probe the gateway usage endpoint when stdin
        // gave us nothing. Skipped in preview mode to avoid network I/O.
        if windows.is_empty() && !ctx.preview_mode {
            if let Some(rate_limits) = self.fetch_usage_windows().await {
                windows = self.build_windows(&rate_limits, now_secs);
            }
        }

        if windows.is_empty() {
            return ComponentOutput::hidden();
        }

        ComponentOutput::new(windows.join(" | "))
            .with_icon(self.select_icon(ctx).unwrap_or_default())
            .with_icon_color(&self.config.base.icon_color)
            .with_text_color(&self.config.base.text_color)
    }

    fn base_config(&self, _ctx: &RenderContext) -> Option<&BaseComponentConfig> {
        Some(&self.config.base)
    }
}

/// TLS verifier that accepts any certificate. Only wired up when the user set
/// `NODE_TLS_REJECT_UNAUTHORIZED=0` (the same escape hatch Node uses), for
/// self-signed gateways. Never used on the default (verifying) path.
#[derive(Debug)]
struct NoCertVerify;

impl rustls::client::danger::ServerCertVerifier for NoCertVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Factory for creating rate limit components.
pub struct RateLimitComponentFactory;

impl ComponentFactory for RateLimitComponentFactory {
    fn create(&self, config: &Config) -> Box<dyn Component> {
        Box::new(RateLimitComponent::new(
            config.components.rate_limit.clone(),
        ))
    }

    fn name(&self) -> &'static str {
        "rate_limit"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::TerminalCapabilities;
    use crate::core::input::{InputData, RateLimitsInfo};
    use std::sync::Arc;

    fn context(input: InputData) -> RenderContext {
        RenderContext {
            input: Arc::new(input),
            config: Arc::new(Config::default()),
            terminal: TerminalCapabilities::default(),
            preview_mode: false,
        }
    }

    #[tokio::test]
    async fn rate_limit_hidden_when_payload_missing() {
        // Disable the auto-probe so this stays hermetic (no env/network).
        let component = RateLimitComponent::new(RateLimitComponentConfig {
            usage_endpoint: Some(String::new()),
            ..RateLimitComponentConfig::default()
        });
        let output = component.render(&context(InputData::default())).await;

        assert!(!output.visible);
    }

    #[tokio::test]
    async fn rate_limit_renders_five_hour_and_seven_day_windows() {
        let component = RateLimitComponent::new(RateLimitComponentConfig {
            show_reset: false,
            ..RateLimitComponentConfig::default()
        });
        let input = InputData {
            rate_limits: Some(RateLimitsInfo {
                five_hour: Some(RateLimitWindow {
                    used_percentage: Some(42.0),
                    resets_at: None,
                }),
                seven_day: Some(RateLimitWindow {
                    used_percentage: Some(7.0),
                    resets_at: None,
                }),
            }),
            ..InputData::default()
        };

        let output = component.render(&context(input)).await;

        assert!(output.visible);
        assert_eq!(output.text, "5h 42% | 7d 7%");
    }

    #[test]
    fn reset_duration_formats_compactly() {
        assert_eq!(RateLimitComponent::format_reset_duration(60, 0), "1m");
        assert_eq!(RateLimitComponent::format_reset_duration(3_660, 0), "1h1m");
        assert_eq!(RateLimitComponent::format_reset_duration(90_000, 0), "1d1h");
    }

    #[test]
    fn parse_usage_json_maps_utilization_and_reset() {
        let json = serde_json::json!({
            "five_hour": {
                "utilization": 6.0,
                "resets_at": "2026-07-01T11:00:00+00:00",
                "status": "allowed"
            },
            "seven_day": { "utilization": 1.0 }
        });

        let info = RateLimitComponent::parse_usage_json(&json);
        assert_eq!(
            info.five_hour.as_ref().and_then(|w| w.used_percentage),
            Some(6.0)
        );
        assert_eq!(
            info.five_hour.as_ref().and_then(|w| w.resets_at),
            Some(1_782_903_600)
        );
        assert_eq!(
            info.seven_day.as_ref().and_then(|w| w.used_percentage),
            Some(1.0)
        );
        assert_eq!(info.seven_day.as_ref().and_then(|w| w.resets_at), None);
    }

    #[test]
    fn parse_usage_json_skips_empty_windows() {
        let json = serde_json::json!({ "five_hour": {} });
        let info = RateLimitComponent::parse_usage_json(&json);
        assert!(info.five_hour.is_none());
        assert!(info.seven_day.is_none());
    }

    #[tokio::test]
    async fn bridge_payload_renders_like_stdin() {
        let component = RateLimitComponent::new(RateLimitComponentConfig {
            show_reset: false,
            ..RateLimitComponentConfig::default()
        });
        let json = serde_json::json!({
            "five_hour": { "utilization": 6.0 },
            "seven_day": { "utilization": 1.0 }
        });
        let info = RateLimitComponent::parse_usage_json(&json);
        let windows = component.build_windows(&info, 0);
        assert_eq!(windows, vec!["5h 6%".to_string(), "7d 1%".to_string()]);
    }

    #[test]
    fn build_usage_url_joins_relative_and_passes_absolute() {
        assert_eq!(
            RateLimitComponent::build_usage_url("/v1/usage", Some("http://bridge:5699/"))
                .as_deref(),
            Some("http://bridge:5699/v1/usage")
        );
        assert_eq!(
            RateLimitComponent::build_usage_url("https://x.test/v1/usage", None).as_deref(),
            Some("https://x.test/v1/usage")
        );
        // Relative endpoint without a base URL cannot resolve.
        assert_eq!(RateLimitComponent::build_usage_url("/v1/usage", None), None);
    }

    #[test]
    fn is_official_base_detects_anthropic() {
        assert!(RateLimitComponent::is_official_base(
            "https://api.anthropic.com"
        ));
        assert!(!RateLimitComponent::is_official_base(
            "https://192.220.15.45:5675"
        ));
    }

    #[test]
    fn usage_from_json_none_when_empty() {
        assert!(RateLimitComponent::usage_from_json(&serde_json::json!({})).is_none());
        assert!(RateLimitComponent::usage_from_json(&serde_json::json!({
            "five_hour": { "utilization": 6.0 }
        }))
        .is_some());
    }

    #[test]
    fn usage_cache_path_is_stable_and_scoped() -> Result<(), Box<dyn std::error::Error>> {
        let a = RateLimitComponent::usage_cache_path("https://gw.example:5675");
        let b = RateLimitComponent::usage_cache_path("https://gw.example:5675");
        let c = RateLimitComponent::usage_cache_path("https://other.example");
        assert_eq!(a, b, "same base URL → same cache path");
        assert_ne!(a, c, "different base URL → different cache path");
        let path = a.ok_or("home dir unavailable")?;
        assert!(path.to_string_lossy().contains("statusline-pro"));
        Ok(())
    }

    #[test]
    fn cache_round_trip_fresh_then_stale() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("rate-usage.json");
        let value = serde_json::json!({ "five_hour": { "utilization": 6.0 } });

        RateLimitComponent::write_cache(&path, &value);
        // Fresh within a generous TTL → returns the stored value.
        assert_eq!(
            RateLimitComponent::read_fresh_cache(&path, Duration::from_secs(60)),
            Some(value)
        );
        // TTL zero → always considered stale → None (triggers a re-probe).
        assert!(RateLimitComponent::read_fresh_cache(&path, Duration::ZERO).is_none());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn tls_skip_honors_node_env() {
        std::env::set_var("NODE_TLS_REJECT_UNAUTHORIZED", "0");
        assert!(RateLimitComponent::tls_skip_enabled(
            "https://self-signed.gw"
        ));
        assert!(!RateLimitComponent::tls_skip_enabled("http://plain.gw"));
        std::env::remove_var("NODE_TLS_REJECT_UNAUTHORIZED");
        assert!(!RateLimitComponent::tls_skip_enabled(
            "https://self-signed.gw"
        ));
    }
}
