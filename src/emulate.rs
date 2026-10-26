//! `set …`: what the page sees of its device and network — viewport, device
//! presets, media features, location, locale, time zone, user agent, extra
//! headers, offline. Each setting becomes CDP calls applied to every tab
//! (and replayed on tabs opened later, see `BrowserSession::set_override`).

use anyhow::{bail, Result};
use serde_json::{json, Value};

/// A device: viewport, pixel ratio, touch, user agent.
pub struct Device {
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub mobile: bool,
    pub ua: &'static str,
}

const IOS_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Mobile/15E148 Safari/604.1";
const IPAD_UA: &str = "Mozilla/5.0 (iPad; CPU OS 18_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Mobile/15E148 Safari/604.1";
const ANDROID_UA: &str = "Mozilla/5.0 (Linux; Android 15; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Mobile Safari/537.36";
const GALAXY_UA: &str = "Mozilla/5.0 (Linux; Android 15; SM-S921B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Mobile Safari/537.36";

/// Common viewports. An empty user agent keeps the browser's own.
pub const DEVICES: &[Device] = &[
    Device { name: "iPhone 15", width: 393, height: 852, scale: 3.0, mobile: true, ua: IOS_UA },
    Device { name: "iPhone 15 Pro Max", width: 430, height: 932, scale: 3.0, mobile: true, ua: IOS_UA },
    Device { name: "iPhone SE", width: 375, height: 667, scale: 2.0, mobile: true, ua: IOS_UA },
    Device { name: "Pixel 8", width: 412, height: 915, scale: 2.625, mobile: true, ua: ANDROID_UA },
    Device { name: "Galaxy S24", width: 360, height: 780, scale: 3.0, mobile: true, ua: GALAXY_UA },
    Device { name: "iPad Mini", width: 768, height: 1024, scale: 2.0, mobile: true, ua: IPAD_UA },
    Device { name: "iPad Pro 11", width: 834, height: 1194, scale: 2.0, mobile: true, ua: IPAD_UA },
    Device { name: "Laptop", width: 1366, height: 768, scale: 1.0, mobile: false, ua: "" },
    Device { name: "Desktop", width: 1920, height: 1080, scale: 1.0, mobile: false, ua: "" },
];

/// A device by name, ignoring case and spaces/dashes (`iphone-15`).
pub fn device(name: &str) -> Option<&'static Device> {
    let key = |s: &str| s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    DEVICES.iter().find(|d| key(d.name) == key(name))
}

/// What one `set` produces.
pub struct Plan {
    /// Override kind, so a later `set` of the same kind replaces it.
    pub kind: &'static str,
    /// CDP (method, params) for every tab; empty = the override is removed.
    pub calls: Vec<(&'static str, Value)>,
    /// Calls that undo an earlier override of this kind first.
    pub reset: Vec<(&'static str, Value)>,
    /// A browser permission to grant (geolocation).
    pub grant: Option<&'static str>,
    /// What the agent is told.
    pub result: Value,
}

fn off(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "off" | "reset" | "none" | "clear" | "default")
}

fn num(raw: &str, what: &str) -> Result<f64> {
    raw.trim().parse::<f64>().map_err(|_| anyhow::anyhow!("set: {what} must be a number, got {raw:?}"))
}

fn metrics(width: u32, height: u32, scale: f64, mobile: bool) -> Value {
    json!({ "width": width, "height": height, "deviceScaleFactor": scale, "mobile": mobile,
            "screenWidth": width, "screenHeight": height })
}

/// Settings Lightpanda 1.0.x answers with success but ignores: refuse them
/// rather than report an emulation that isn't there.
fn lightpanda_refuses(what: &str, values: &[String]) -> Option<String> {
    let first = values.first().map(String::as_str).unwrap_or("");
    if off(first) {
        return None;
    }
    match what {
        "media" => Some("the lightpanda engine does not emulate media features (prefers-color-scheme, print)".into()),
        "offline" => Some("the lightpanda engine does not emulate going offline".into()),
        "locale" | "timezone" | "tz" => Some(format!(
            "the lightpanda engine takes its {what} at launch only (`lightpanda serve --{}`)",
            if what == "locale" { "locale" } else { "timezone" }
        )),
        "ua" | "user-agent" | "useragent" if values.join(" ").contains("Mozilla") => {
            Some("the lightpanda engine refuses user agents that contain \"Mozilla\"".into())
        }
        _ => None,
    }
}

/// Turn `set <what> <values…>` into CDP calls (`lightpanda`: only what that
/// engine really applies).
pub fn plan(
    what: &str,
    values: &[String],
    scale: Option<f64>,
    mobile: bool,
    accuracy: Option<f64>,
    lightpanda: bool,
) -> Result<Plan> {
    let first = values.first().map(String::as_str).unwrap_or("");
    if lightpanda {
        if let Some(why) = lightpanda_refuses(what, values) {
            bail!("set {what}: {why}");
        }
        // Never on, so nothing to undo: the engine knows neither call.
        if what == "offline" {
            return Ok(Plan {
                kind: "offline",
                calls: vec![],
                reset: vec![],
                grant: None,
                result: json!({ "offline": false }),
            });
        }
    }
    let reset_metrics = vec![("Emulation.clearDeviceMetricsOverride", json!({}))];
    Ok(match what {
        "viewport" => {
            if off(first) {
                return Ok(Plan { kind: "viewport", calls: vec![], reset: reset_metrics, grant: None, result: json!({ "viewport": "default" }) });
            }
            let (w, h) = match values {
                [w, h] => (num(w, "width")? as u32, num(h, "height")? as u32),
                [wh] if wh.contains('x') => {
                    let (w, h) = wh.split_once('x').expect("checked");
                    (num(w, "width")? as u32, num(h, "height")? as u32)
                }
                _ => bail!("set viewport <width> <height> [--scale <dpr>] [--mobile] (or `off`)"),
            };
            let scale = scale.unwrap_or(1.0);
            Plan {
                kind: "viewport",
                calls: vec![("Emulation.setDeviceMetricsOverride", metrics(w, h, scale, mobile))],
                reset: vec![],
                grant: None,
                result: json!({ "viewport": [w, h], "scale": scale, "mobile": mobile }),
            }
        }
        "device" => {
            if first.is_empty() || first == "list" {
                bail!(
                    "set device <name>; known: {}",
                    DEVICES.iter().map(|d| d.name).collect::<Vec<_>>().join(", ")
                );
            }
            let name = values.join(" ");
            if off(&name) {
                return Ok(Plan {
                    kind: "viewport",
                    calls: vec![],
                    reset: vec![
                        ("Emulation.clearDeviceMetricsOverride", json!({})),
                        ("Emulation.setTouchEmulationEnabled", json!({ "enabled": false })),
                    ],
                    grant: None,
                    result: json!({ "device": "default" }),
                });
            }
            let d = device(&name).ok_or_else(|| {
                anyhow::anyhow!(
                    "set device: unknown {name:?}; known: {}",
                    DEVICES.iter().map(|d| d.name).collect::<Vec<_>>().join(", ")
                )
            })?;
            let mut calls = vec![("Emulation.setDeviceMetricsOverride", metrics(d.width, d.height, d.scale, d.mobile))];
            let mut result = json!({ "device": d.name, "viewport": [d.width, d.height], "scale": d.scale, "mobile": d.mobile });
            if lightpanda {
                // Lightpanda applies the viewport only: no touch, and it
                // won't send a browser's user agent.
                result["note"] = json!("lightpanda: viewport only (its own user agent, no touch)");
            } else {
                calls.push((
                    "Emulation.setTouchEmulationEnabled",
                    json!({ "enabled": d.mobile, "maxTouchPoints": if d.mobile { 5 } else { 0 } }),
                ));
                if !d.ua.is_empty() {
                    calls.push(("Emulation.setUserAgentOverride", json!({ "userAgent": d.ua })));
                }
            }
            Plan { kind: "viewport", calls, reset: vec![], grant: None, result }
        }
        "media" => {
            let mut media = "";
            let mut features = Vec::new();
            if !off(first) {
                for v in values {
                    match v.to_ascii_lowercase().as_str() {
                        "dark" | "light" => features.push(json!({ "name": "prefers-color-scheme", "value": v.to_ascii_lowercase() })),
                        "print" | "screen" => media = if v.eq_ignore_ascii_case("print") { "print" } else { "screen" },
                        "reduced-motion" | "reduce" => features.push(json!({ "name": "prefers-reduced-motion", "value": "reduce" })),
                        "forced-colors" => features.push(json!({ "name": "forced-colors", "value": "active" })),
                        other => bail!("set media takes dark|light|print|screen|reduced-motion|forced-colors|off, got {other:?}"),
                    }
                }
            }
            let reset = !media.is_empty() || !features.is_empty();
            Plan {
                kind: "media",
                calls: if reset { vec![("Emulation.setEmulatedMedia", json!({ "media": media, "features": features }))] } else { vec![] },
                reset: vec![("Emulation.setEmulatedMedia", json!({ "media": "", "features": [] }))],
                grant: None,
                result: json!({ "media": if reset { values.join(" ") } else { "default".into() } }),
            }
        }
        "geo" | "geolocation" | "location" => {
            if off(first) {
                return Ok(Plan {
                    kind: "geo",
                    calls: vec![],
                    reset: vec![("Emulation.clearGeolocationOverride", json!({}))],
                    grant: None,
                    result: json!({ "geo": "default" }),
                });
            }
            let [lat, lon] = values else { bail!("set geo <latitude> <longitude> [--accuracy <m>] (or `off`)") };
            let (lat, lon) = (num(lat, "latitude")?, num(lon, "longitude")?);
            if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                bail!("set geo: latitude must be in -90..90 and longitude in -180..180");
            }
            let accuracy = accuracy.unwrap_or(50.0);
            Plan {
                kind: "geo",
                calls: vec![("Emulation.setGeolocationOverride", json!({ "latitude": lat, "longitude": lon, "accuracy": accuracy }))],
                reset: vec![],
                grant: Some("geolocation"),
                result: json!({ "geo": [lat, lon], "accuracy": accuracy }),
            }
        }
        "offline" => {
            let on = match first.to_ascii_lowercase().as_str() {
                "" | "on" | "true" | "1" => true,
                "off" | "false" | "0" => false,
                other => bail!("set offline takes on|off, got {other:?}"),
            };
            // Requests fail by a rule for every URL (an empty pattern), and
            // `navigator.onLine` follows from a state of its own: the two
            // calls that replace the deprecated `emulateNetworkConditions`.
            let state = |offline: bool| {
                json!({ "offline": offline, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1 })
            };
            Plan {
                kind: "offline",
                calls: if on {
                    vec![
                        (
                            "Network.emulateNetworkConditionsByRule",
                            json!({ "matchedNetworkConditions": [{
                                "urlPattern": "", "offline": true, "latency": 0,
                                "downloadThroughput": -1, "uploadThroughput": -1
                            }] }),
                        ),
                        ("Network.overrideNetworkState", state(true)),
                    ]
                } else {
                    vec![]
                },
                reset: vec![
                    ("Network.emulateNetworkConditionsByRule", json!({ "matchedNetworkConditions": [] })),
                    ("Network.overrideNetworkState", state(false)),
                ],
                grant: None,
                result: json!({ "offline": on }),
            }
        }
        "headers" | "header" => {
            if off(first) {
                return Ok(Plan {
                    kind: "headers",
                    calls: vec![],
                    reset: vec![("Network.setExtraHTTPHeaders", json!({ "headers": {} }))],
                    grant: None,
                    result: json!({ "headers": {} }),
                });
            }
            // `{"X-A": "1"}` or `X-A: 1` pairs.
            let headers: serde_json::Map<String, Value> = if first.trim_start().starts_with('{') {
                match serde_json::from_str::<Value>(&values.join(" "))? {
                    Value::Object(map) => map.into_iter().map(|(k, v)| (k, json!(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))).collect(),
                    _ => bail!("set headers: give a JSON object or `Name: value` pairs"),
                }
            } else {
                // `X-A: 1` arrives as two shell words: rejoin a bare value
                // with the `Name:` before it.
                let mut pairs: Vec<String> = Vec::new();
                for v in values {
                    match pairs.last_mut() {
                        Some(last) if last.trim_end().ends_with(':') => {
                            last.push(' ');
                            last.push_str(v);
                        }
                        _ => pairs.push(v.clone()),
                    }
                }
                pairs
                    .iter()
                    .map(|h| {
                        let (k, v) = h.split_once(':').ok_or_else(|| anyhow::anyhow!("set headers: {h:?} is not `Name: value`"))?;
                        Ok((k.trim().to_string(), json!(v.trim())))
                    })
                    .collect::<Result<_>>()?
            };
            Plan {
                kind: "headers",
                calls: vec![("Network.setExtraHTTPHeaders", json!({ "headers": headers }))],
                reset: vec![],
                grant: None,
                result: json!({ "headers": headers }),
            }
        }
        "ua" | "user-agent" | "useragent" => {
            let ua = values.join(" ");
            if ua.is_empty() {
                bail!("set ua <user agent string> (or `off`)");
            }
            Plan {
                kind: "ua",
                calls: if off(&ua) { vec![] } else { vec![("Emulation.setUserAgentOverride", json!({ "userAgent": ua }))] },
                // Chrome has no "clear": an empty string restores its own.
                reset: if off(&ua) { vec![("Emulation.setUserAgentOverride", json!({ "userAgent": "" }))] } else { vec![] },
                grant: None,
                result: json!({ "ua": if off(&ua) { "default".to_string() } else { ua } }),
            }
        }
        "locale" => {
            if first.is_empty() {
                bail!("set locale <BCP 47 tag, e.g. de-DE> (or `off`)");
            }
            Plan {
                kind: "locale",
                calls: if off(first) { vec![] } else { vec![("Emulation.setLocaleOverride", json!({ "locale": first }))] },
                reset: if off(first) { vec![("Emulation.setLocaleOverride", json!({}))] } else { vec![] },
                grant: None,
                result: json!({ "locale": if off(first) { "default" } else { first } }),
            }
        }
        "timezone" | "tz" => {
            if first.is_empty() {
                bail!("set timezone <IANA zone, e.g. Europe/Berlin> (or `off`)");
            }
            Plan {
                kind: "timezone",
                calls: if off(first) { vec![] } else { vec![("Emulation.setTimezoneOverride", json!({ "timezoneId": first }))] },
                reset: if off(first) { vec![("Emulation.setTimezoneOverride", json!({ "timezoneId": "" }))] } else { vec![] },
                grant: None,
                result: json!({ "timezone": if off(first) { "default" } else { first } }),
            }
        }
        other => bail!("set: unknown setting {other:?}; settings: viewport, device, media, geo, offline, headers, ua, locale, timezone"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn devices_by_loose_name() {
        assert_eq!(device("iphone-15").unwrap().name, "iPhone 15");
        assert_eq!(device("Pixel 8").unwrap().width, 412);
        assert!(device("nokia 3310").is_none());
    }

    #[test]
    fn plans() {
        let p = plan("viewport", &v(&["390", "844"]), Some(3.0), true, None, false).unwrap();
        assert_eq!(p.calls[0].1["width"], 390);
        assert_eq!(p.calls[0].1["deviceScaleFactor"], 3.0);
        assert_eq!(plan("viewport", &v(&["1280x720"]), None, false, None, false).unwrap().calls[0].1["height"], 720);
        let p = plan("device", &v(&["iPhone", "15"]), None, false, None, false).unwrap();
        assert_eq!(p.kind, "viewport");
        assert_eq!(p.calls.len(), 3);
        let p = plan("media", &v(&["dark", "print"]), None, false, None, false).unwrap();
        assert_eq!(p.calls[0].1["media"], "print");
        assert_eq!(p.calls[0].1["features"][0]["value"], "dark");
        assert!(plan("media", &v(&["off"]), None, false, None, false).unwrap().calls.is_empty());
        let p = plan("geo", &v(&["52.52", "13.405"]), None, false, None, false).unwrap();
        assert_eq!(p.grant, Some("geolocation"));
        assert!(plan("geo", &v(&["91", "0"]), None, false, None, false).is_err());
        let p = plan("headers", &v(&["X-A: 1", "X-B:two"]), None, false, None, false).unwrap();
        assert_eq!(p.calls[0].1["headers"]["X-B"], "two");
        let p = plan("headers", &v(&[r#"{"X-C": 3}"#]), None, false, None, false).unwrap();
        assert_eq!(p.calls[0].1["headers"]["X-C"], "3");
        assert!(plan("offline", &v(&["off"]), None, false, None, false).unwrap().calls.is_empty());
        // Not the deprecated `emulateNetworkConditions`: a rule for every URL
        // and the navigator state, both undone by `off`.
        let p = plan("offline", &v(&["on"]), None, false, None, false).unwrap();
        let methods: Vec<&str> = p.calls.iter().chain(p.reset.iter()).map(|(m, _)| *m).collect();
        assert_eq!(
            methods,
            [
                "Network.emulateNetworkConditionsByRule",
                "Network.overrideNetworkState",
                "Network.emulateNetworkConditionsByRule",
                "Network.overrideNetworkState"
            ]
        );
        assert_eq!(p.calls[0].1["matchedNetworkConditions"][0]["offline"], true);
        assert_eq!(p.calls[0].1["matchedNetworkConditions"][0]["urlPattern"], "");
        assert_eq!(p.reset[0].1["matchedNetworkConditions"], serde_json::json!([]));
        assert!(plan("wat", &v(&[]), None, false, None, false).is_err());
        let p = plan("headers", &v(&["X-D:", "4"]), None, false, None, false).unwrap();
        assert_eq!(p.calls[0].1["headers"]["X-D"], "4");
        // Lightpanda: refused rather than silently ignored; device = viewport only.
        assert!(plan("media", &v(&["dark"]), None, false, None, true).is_err());
        assert!(plan("timezone", &v(&["Asia/Tokyo"]), None, false, None, true).is_err());
        assert!(plan("ua", &v(&["Mozilla/5.0"]), None, false, None, true).is_err());
        assert!(plan("media", &v(&["off"]), None, false, None, true).is_ok());
        assert!(plan("offline", &v(&["on"]), None, false, None, true).is_err());
        let p = plan("offline", &v(&["off"]), None, false, None, true).unwrap();
        assert!(p.calls.is_empty() && p.reset.is_empty(), "nothing the engine would refuse");
        assert_eq!(plan("device", &v(&["Pixel 8"]), None, false, None, true).unwrap().calls.len(), 1);
    }
}
