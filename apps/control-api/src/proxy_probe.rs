//! Proxy connectivity / quality probes (sub2api-style IP management).
//!
//! - Test: exit IP via ip-api / httpbin through the proxy + latency
//! - Quality: base connectivity + reachability of common AI API endpoints

use crate::proxy::{GetProxyRequest, ProxyStore};
use halolake_control_plane::ManagementError;
use reqwest::{Client, Proxy, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use service_async::Service;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const QUALITY_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BODY: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProxyTestResult {
    pub success:      bool,
    pub message:      String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms:   Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_address:   Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city:         Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region:       Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country:      Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProxyQualityCheckItem {
    pub target:      String,
    /// pass | warn | fail | challenge
    pub status:      String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default)]
    pub latency_ms:  i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message:     Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cf_ray:      Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProxyQualityCheckResult {
    pub proxy_id:        u64,
    pub score:           i32,
    pub grade:           String,
    pub summary:         String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_ip:         Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country:         Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code:    Option<String>,
    #[serde(default)]
    pub base_latency_ms: i64,
    pub passed_count:    i32,
    pub warn_count:      i32,
    pub failed_count:    i32,
    pub challenge_count: i32,
    pub checked_at:      i64,
    pub items:           Vec<ProxyQualityCheckItem>,
}

#[derive(Debug, Clone, Default)]
struct ExitInfo {
    ip:           String,
    city:         String,
    region:       String,
    country:      String,
    country_code: String,
}

pub(crate) async fn test_proxy(
    proxies: &ProxyStore,
    id: u64,
) -> Result<ProxyTestResult, ManagementError> {
    let proxy = proxies.call(GetProxyRequest { id }).await?;
    match probe_exit(&proxy.url).await {
        Ok((info, latency_ms)) => Ok(ProxyTestResult {
            success:      true,
            message:      "Proxy is accessible".into(),
            latency_ms:   Some(latency_ms),
            ip_address:   Some(info.ip).filter(|s| !s.is_empty()),
            city:         Some(info.city).filter(|s| !s.is_empty()),
            region:       Some(info.region).filter(|s| !s.is_empty()),
            country:      Some(info.country).filter(|s| !s.is_empty()),
            country_code: Some(info.country_code).filter(|s| !s.is_empty()),
        }),
        Err(err) => Ok(ProxyTestResult {
            success:      false,
            message:      err,
            latency_ms:   None,
            ip_address:   None,
            city:         None,
            region:       None,
            country:      None,
            country_code: None,
        }),
    }
}

pub(crate) async fn check_proxy_quality(
    proxies: &ProxyStore,
    id: u64,
) -> Result<ProxyQualityCheckResult, ManagementError> {
    let proxy = proxies.call(GetProxyRequest { id }).await?;
    let checked_at = now_unix();
    let mut result = ProxyQualityCheckResult {
        proxy_id: id,
        score: 100,
        grade: "A".into(),
        summary: String::new(),
        exit_ip: None,
        country: None,
        country_code: None,
        base_latency_ms: 0,
        passed_count: 0,
        warn_count: 0,
        failed_count: 0,
        challenge_count: 0,
        checked_at,
        items: Vec::new(),
    };

    match probe_exit(&proxy.url).await {
        Ok((info, latency_ms)) => {
            result.exit_ip = Some(info.ip.clone()).filter(|s| !s.is_empty());
            result.country = Some(info.country.clone()).filter(|s| !s.is_empty());
            result.country_code = Some(info.country_code.clone()).filter(|s| !s.is_empty());
            result.base_latency_ms = latency_ms;
            result.items.push(ProxyQualityCheckItem {
                target: "base_connectivity".into(),
                status: "pass".into(),
                http_status: None,
                latency_ms,
                message: Some("代理出口连通正常".into()),
                cf_ray: None,
            });
            result.passed_count += 1;
        }
        Err(err) => {
            result.items.push(ProxyQualityCheckItem {
                target:      "base_connectivity".into(),
                status:      "fail".into(),
                http_status: None,
                latency_ms:  0,
                message:     Some(err),
                cf_ray:      None,
            });
            result.failed_count += 1;
            finalize_quality(&mut result);
            return Ok(result);
        }
    }

    let client = match proxy_client(&proxy.url, QUALITY_TIMEOUT) {
        Ok(c) => c,
        Err(err) => {
            result.items.push(ProxyQualityCheckItem {
                target:      "http_client".into(),
                status:      "fail".into(),
                http_status: None,
                latency_ms:  0,
                message:     Some(format!("创建检测客户端失败: {err}")),
                cf_ray:      None,
            });
            result.failed_count += 1;
            finalize_quality(&mut result);
            return Ok(result);
        }
    };

    for target in quality_targets() {
        let item = run_quality_target(&client, &target).await;
        match item.status.as_str() {
            "pass" => result.passed_count += 1,
            "warn" => result.warn_count += 1,
            "challenge" => result.challenge_count += 1,
            _ => result.failed_count += 1,
        }
        result.items.push(item);
    }

    finalize_quality(&mut result);
    Ok(result)
}

struct QualityTarget {
    name:    &'static str,
    url:     &'static str,
    method:  reqwest::Method,
    allowed: &'static [u16],
}

fn quality_targets() -> [QualityTarget; 3] {
    [
        QualityTarget {
            name:    "openai",
            url:     "https://api.openai.com/v1/models",
            method:  reqwest::Method::GET,
            allowed: &[401],
        },
        QualityTarget {
            name:    "anthropic",
            url:     "https://api.anthropic.com/v1/messages",
            method:  reqwest::Method::GET,
            allowed: &[401, 405, 404, 400],
        },
        QualityTarget {
            name:    "gemini",
            url:     "https://generativelanguage.googleapis.com/$discovery/rest?version=v1beta",
            method:  reqwest::Method::GET,
            allowed: &[200],
        },
    ]
}

async fn run_quality_target(client: &Client, target: &QualityTarget) -> ProxyQualityCheckItem {
    let mut item = ProxyQualityCheckItem {
        target:      target.name.into(),
        status:      "fail".into(),
        http_status: None,
        latency_ms:  0,
        message:     None,
        cf_ray:      None,
    };
    let start = Instant::now();
    let req = match client
        .request(target.method.clone(), target.url)
        .header("Accept", "application/json,text/html,*/*")
        .header(
            "User-Agent",
            "halolake-proxy-quality/1.0 (+https://github.com/Tokimorphling/halolake)",
        )
        .build()
    {
        Ok(r) => r,
        Err(err) => {
            item.message = Some(format!("构建请求失败: {err}"));
            return item;
        }
    };
    match client.execute(req).await {
        Ok(resp) => {
            item.latency_ms = start.elapsed().as_millis() as i64;
            let status = resp.status().as_u16();
            item.http_status = Some(status);
            let headers = resp.headers().clone();
            let body = resp.bytes().await.unwrap_or_default();
            let body_str = String::from_utf8_lossy(&body);
            if is_cloudflare_challenge(status, &headers, &body_str) {
                item.status = "challenge".into();
                item.cf_ray = extract_cf_ray(&headers);
                item.message = Some("命中 Cloudflare challenge".into());
                return item;
            }
            if target.allowed.contains(&status) {
                item.status = "pass".into();
                item.message = Some(if (200..300).contains(&status) {
                    format!("HTTP {status}")
                } else {
                    format!("HTTP {status}（目标可达）")
                });
                return item;
            }
            if status == 429 {
                item.status = "warn".into();
                item.message = Some("目标返回 429，可能存在频控".into());
                return item;
            }
            item.message = Some(format!("非预期状态码: {status}"));
            item
        }
        Err(err) => {
            item.latency_ms = start.elapsed().as_millis() as i64;
            item.message = Some(format!("请求失败: {err}"));
            item
        }
    }
}

fn finalize_quality(result: &mut ProxyQualityCheckResult) {
    let mut score =
        100 - result.warn_count * 10 - result.failed_count * 22 - result.challenge_count * 30;
    if score < 0 {
        score = 0;
    }
    result.score = score;
    result.grade = match score {
        90..=100 => "A",
        75..=89 => "B",
        60..=74 => "C",
        40..=59 => "D",
        _ => "F",
    }
    .into();
    result.summary = format!(
        "通过 {} 项，告警 {} 项，失败 {} 项，挑战 {} 项",
        result.passed_count, result.warn_count, result.failed_count, result.challenge_count
    );
}

async fn probe_exit(proxy_url: &str) -> Result<(ExitInfo, i64), String> {
    let client = proxy_client(proxy_url, PROBE_TIMEOUT)?;
    let mut last_err = String::from("all probe URLs failed");
    // Prefer ip-api (geo), fallback httpbin (ip only). Use HTTP for proxies that filter HTTPS.
    for (url, parser) in [
        ("http://ip-api.com/json/?lang=zh-CN", "ip-api"),
        ("http://httpbin.org/ip", "httpbin"),
        ("https://api.ipify.org?format=json", "ipify"),
    ] {
        match probe_one(&client, url, parser).await {
            Ok(v) => return Ok(v),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

async fn probe_one(client: &Client, url: &str, parser: &str) -> Result<(ExitInfo, i64), String> {
    let start = Instant::now();
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("proxy connection failed: {e}"))?;
    let latency_ms = start.elapsed().as_millis() as i64;
    if resp.status() != StatusCode::OK {
        return Err(format!("request failed with status: {}", resp.status()));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("failed to read response: {e}"))?;
    if body.len() as u64 > MAX_BODY {
        return Err("proxy probe response exceeds limit".into());
    }
    let info = match parser {
        "ip-api" => parse_ip_api(&body)?,
        "httpbin" => parse_httpbin(&body)?,
        "ipify" => parse_ipify(&body)?,
        _ => return Err(format!("unknown parser: {parser}")),
    };
    Ok((info, latency_ms))
}

fn parse_ip_api(body: &[u8]) -> Result<ExitInfo, String> {
    #[derive(Deserialize)]
    struct IpApi {
        status:       Option<String>,
        message:      Option<String>,
        query:        Option<String>,
        city:         Option<String>,
        region:       Option<String>,
        #[serde(rename = "regionName")]
        region_name:  Option<String>,
        country:      Option<String>,
        #[serde(rename = "countryCode")]
        country_code: Option<String>,
    }
    let ip: IpApi =
        serde_json::from_slice(body).map_err(|e| format!("failed to parse response: {e}"))?;
    if !ip
        .status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("success"))
    {
        return Err(format!(
            "ip-api request failed: {}",
            ip.message.unwrap_or_else(|| "unknown".into())
        ));
    }
    Ok(ExitInfo {
        ip:           ip.query.unwrap_or_default(),
        city:         ip.city.unwrap_or_default(),
        region:       ip.region_name.or(ip.region).unwrap_or_default(),
        country:      ip.country.unwrap_or_default(),
        country_code: ip.country_code.unwrap_or_default(),
    })
}

fn parse_httpbin(body: &[u8]) -> Result<ExitInfo, String> {
    let v: JsonValue = serde_json::from_slice(body)
        .map_err(|e| format!("failed to parse httpbin response: {e}"))?;
    let origin = v
        .get("origin")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if origin.is_empty() {
        return Err("httpbin: no IP found in response".into());
    }
    Ok(ExitInfo {
        ip: origin,
        ..ExitInfo::default()
    })
}

fn parse_ipify(body: &[u8]) -> Result<ExitInfo, String> {
    let v: JsonValue =
        serde_json::from_slice(body).map_err(|e| format!("failed to parse ipify response: {e}"))?;
    let ip = v
        .get("ip")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if ip.is_empty() {
        return Err("ipify: no IP found".into());
    }
    Ok(ExitInfo {
        ip,
        ..ExitInfo::default()
    })
}

fn proxy_client(proxy_url: &str, timeout: Duration) -> Result<Client, String> {
    let proxy = Proxy::all(proxy_url).map_err(|e| format!("invalid proxy url: {e}"))?;
    Client::builder()
        .proxy(proxy)
        .timeout(timeout)
        .connect_timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("failed to create proxy client: {e}"))
}

fn is_cloudflare_challenge(status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> bool {
    let server = headers
        .get("server")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let has_cf = headers.contains_key("cf-ray") || server.contains("cloudflare");
    if !has_cf {
        return false;
    }
    if status == 403 || status == 503 {
        let b = body.to_ascii_lowercase();
        return b.contains("just a moment")
            || b.contains("cf-browser-verification")
            || b.contains("challenge-platform")
            || b.contains("attention required");
    }
    false
}

fn extract_cf_ray(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("cf-ray")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grades_from_score() {
        let mut r = ProxyQualityCheckResult {
            proxy_id:        1,
            score:           0,
            grade:           String::new(),
            summary:         String::new(),
            exit_ip:         None,
            country:         None,
            country_code:    None,
            base_latency_ms: 0,
            passed_count:    1,
            warn_count:      1,
            failed_count:    1,
            challenge_count: 0,
            checked_at:      0,
            items:           vec![],
        };
        finalize_quality(&mut r);
        // 100 - 10 - 22 = 68 -> C
        assert_eq!(r.score, 68);
        assert_eq!(r.grade, "C");
    }

    #[test]
    fn parses_ipify() {
        let info = parse_ipify(br#"{"ip":"1.2.3.4"}"#).unwrap();
        assert_eq!(info.ip, "1.2.3.4");
    }
}
