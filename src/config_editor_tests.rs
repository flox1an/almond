//! Drift and fidelity tests for `src/config-editor.html`.
//!
//! `config-editor.html` is a hand-written, build-free artifact (see
//! `docs/plans/2026-08-11-config-editor-spec.md`). Its `ALMOND_SCHEMA`
//! literal is a second, independently maintained description of the same
//! variables `Config::from_map` reads. Nothing here generates that literal;
//! these tests only prove it has not drifted from `src/config.rs`, by
//! extracting it and exercising `Config::from_map` with each field's
//! `default` and `probe` value.
//!
//! Also covered: the CSP meta tag's script/style hashes actually match the
//! file's inline `<script>`/`<style>` content, no network-capable APIs made
//! it into the script, and a handful of `.env` fixtures under
//! `testdata/config-editor/` parse the way dotenvy 0.15.7 really parses
//! them (used both to pin dotenvy's documented behaviour and, for the
//! quoting/duplicates/multiline cases, to mirror the assertions in the
//! file's own `#selftest` harness).

use std::collections::{BTreeSet, HashMap};

use base64::Engine as _;
use regex::Regex;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::Config;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct SchemaField {
    name: String,
    empty: String,
    default: Option<String>,
    probe: String,
    #[serde(default)]
    conditional_default: bool,
}

fn load_html() -> &'static str {
    include_str!("config-editor.html")
}

/// Returns the substring of `text` starting right after the first
/// occurrence of `start` and ending right before the following occurrence
/// of `end`. Panics if either marker is missing — a missing marker means
/// the file was restructured in a way these tests need to know about.
fn extract_between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let after_start = text
        .find(start)
        .unwrap_or_else(|| panic!("marker not found: {start:?}"))
        + start.len();
    let rel_end = text[after_start..]
        .find(end)
        .unwrap_or_else(|| panic!("marker not found after {start:?}: {end:?}"));
    &text[after_start..after_start + rel_end]
}

/// Finds the JSON array starting at the first `[` in `text` and returns the
/// slice up to its matching `]`, tracking bracket depth outside of JSON
/// string literals so a `[` or `]` inside a help string can't confuse it.
fn extract_balanced_array(text: &str) -> &str {
    let start = text
        .find('[')
        .expect("no '[' found where the schema array should start");
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return &text[start..=i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced [ ] while scanning the ALMOND_SCHEMA literal");
}

fn load_schema() -> Vec<SchemaField> {
    let html = load_html();
    let region = extract_between(
        html,
        "/* ALMOND_SCHEMA_JSON_START */",
        "/* ALMOND_SCHEMA_JSON_END */",
    );
    let array_text = extract_balanced_array(region);
    serde_json::from_str(array_text).expect("ALMOND_SCHEMA in config-editor.html is not valid JSON")
}

fn try_config(pairs: &[(&str, &str)]) -> Result<Config, crate::config::ConfigError> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Config::from_map(&map)
}

fn configs_differ(
    a: &Result<Config, crate::config::ConfigError>,
    b: &Result<Config, crate::config::ConfigError>,
) -> bool {
    match (a, b) {
        (Ok(x), Ok(y)) => x != y,
        (Err(_), Err(_)) => false,
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Drift test 1: literal defaults
// ---------------------------------------------------------------------------

#[test]
fn schema_defaults_match_config_rs() {
    let schema = load_schema();
    let baseline =
        Config::from_map(&HashMap::new()).expect("Config::from_map(empty) must always succeed");
    for f in &schema {
        if f.conditional_default {
            continue; // covered by public_url_conditional_default_matches_config_rs
        }
        let Some(default) = &f.default else {
            continue; // no literal default to test (opt/list-shaped fields)
        };
        let with_default = try_config(&[(&f.name, default)]).unwrap_or_else(|e| {
            panic!(
                "{}: setting the schema's claimed default {default:?} failed to parse: {e}",
                f.name
            )
        });
        assert_eq!(
            with_default, baseline,
            "{}: explicitly setting default={default:?} must equal leaving it unset",
            f.name
        );
    }
}

#[test]
fn public_url_conditional_default_matches_config_rs() {
    let http_default = Config::from_map(&HashMap::new()).unwrap();
    assert_eq!(http_default.public_url, "http://127.0.0.1:3000");

    let mut with_https = HashMap::new();
    with_https.insert("ENABLE_HTTPS".to_string(), "true".to_string());
    let https_default = Config::from_map(&with_https).unwrap();
    assert_eq!(https_default.public_url, "https://127.0.0.1:3000");
}

// ---------------------------------------------------------------------------
// Drift test 2: every field is actually wired up
// ---------------------------------------------------------------------------

#[test]
fn every_schema_field_is_actually_wired_up() {
    let schema = load_schema();
    let baseline = try_config(&[]);
    for f in &schema {
        let probed = try_config(&[(&f.name, &f.probe)]);
        assert!(
            configs_differ(&baseline, &probed),
            "{}: probe value {:?} did not change the effective configuration \u{2014} the field may be misspelled or dead in the schema",
            f.name,
            f.probe
        );
    }
}

// ---------------------------------------------------------------------------
// Drift test 3: empty-value family
// ---------------------------------------------------------------------------

#[test]
fn empty_value_behaviour_matches_schema_claim() {
    let schema = load_schema();
    let baseline = Config::from_map(&HashMap::new()).unwrap();
    for f in &schema {
        let result = try_config(&[(&f.name, "")]);
        match f.empty.as_str() {
            "hard_error" => {
                assert!(
                    result.is_err(),
                    "{}: schema claims empty is a hard error, but Config::from_map accepted it",
                    f.name
                );
            }
            "like_unset" => {
                let cfg = result.unwrap_or_else(|e| {
                    panic!(
                        "{}: schema claims empty behaves like unset, but it errored: {e}",
                        f.name
                    )
                });
                assert_eq!(
                    cfg, baseline,
                    "{}: empty value should equal leaving the field unset",
                    f.name
                );
            }
            "empty_value" => {
                let cfg = result.unwrap_or_else(|e| {
                    panic!(
                        "{}: schema claims empty is a literal empty value, but it errored: {e}",
                        f.name
                    )
                });
                assert_ne!(
                    cfg, baseline,
                    "{}: empty value should differ from the default",
                    f.name
                );
            }
            other => panic!("{}: unknown `empty` behaviour {other:?} in schema", f.name),
        }
    }
}

// ---------------------------------------------------------------------------
// Drift test 4: completeness
// ---------------------------------------------------------------------------

#[test]
fn schema_names_match_config_rs_literals() {
    let schema = load_schema();
    let schema_names: BTreeSet<String> = schema.iter().map(|f| f.name.clone()).collect();

    let config_src = include_str!("config.rs");
    let re = Regex::new(r#""([A-Z][A-Z0-9_]*)""#).unwrap();
    let code_names: BTreeSet<String> = re
        .captures_iter(config_src)
        .map(|c| c[1].to_string())
        .collect();

    assert_eq!(
        schema_names, code_names,
        "config-editor.html's ALMOND_SCHEMA and the env-var literals in config.rs have diverged"
    );
}

// ---------------------------------------------------------------------------
// CSP hash fidelity
// ---------------------------------------------------------------------------

fn sha256_base64(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

#[test]
fn csp_hashes_match_inline_script_and_style() {
    let html = load_html();
    let style = extract_between(html, "<style>", "</style>");
    let script = extract_between(html, "<script>", "</script>");

    let style_hash = sha256_base64(style);
    let script_hash = sha256_base64(script);

    assert!(
        html.contains(&format!("script-src 'sha256-{script_hash}'")),
        "CSP meta tag's script-src hash is stale \u{2014} recompute it. Expected sha256-{script_hash}"
    );
    assert!(
        html.contains(&format!("style-src 'sha256-{style_hash}'")),
        "CSP meta tag's style-src hash is stale \u{2014} recompute it. Expected sha256-{style_hash}"
    );
}

// ---------------------------------------------------------------------------
// No network-capable code, no external resources
// ---------------------------------------------------------------------------

#[test]
fn no_network_capable_apis_in_script() {
    let html = load_html();
    let script = extract_between(html, "<script>", "</script>");
    for forbidden in [
        "fetch(",
        "XMLHttpRequest",
        "WebSocket(",
        "EventSource(",
        "sendBeacon",
        "import(",
    ] {
        assert!(
            !script.contains(forbidden),
            "forbidden network-capable API found in the inline script: {forbidden}"
        );
    }
}

#[test]
fn no_external_resource_references() {
    let html = load_html();
    for forbidden in [
        "src=\"http",
        "href=\"http",
        "src='http",
        "href='http",
        "url(http",
        "<link ",
        "<script src",
    ] {
        assert!(
            !html.contains(forbidden),
            "external resource reference found: {forbidden}"
        );
    }
}

// ---------------------------------------------------------------------------
// Fixture parsing: pins dotenvy 0.15.7's real behaviour, and mirrors the
// assertions in config-editor.html's own #selftest harness for the cases
// that harness can exercise client-side.
// ---------------------------------------------------------------------------

fn parse_fixture(bytes: &[u8]) -> Vec<dotenvy::Result<(String, String)>> {
    dotenvy::Iter::new(std::io::Cursor::new(bytes)).collect()
}

fn tuples(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn fixture_quoting_matches_dotenvy() {
    let bytes = include_bytes!("../testdata/config-editor/quoting.env");
    let got: Vec<(String, String)> = parse_fixture(bytes)
        .into_iter()
        .map(|r| r.expect("quoting.env should parse without error"))
        .collect();
    assert_eq!(
        got,
        tuples(&[
            ("KEY1", "bare"),
            ("KEY2", "double quoted"),
            ("KEY3", "single quoted"),
            ("KEY4", ""),
            ("KEY5", "line 1\nline 2"),
            ("KEY6", "quote \" inside"),
            ("KEY7", "unescaped $ and \\n stay literal"),
            ("KEY8", "trailing"),
            ("KEY9", "exported"),
        ])
    );
}

#[test]
fn fixture_duplicates_first_wins() {
    let bytes = include_bytes!("../testdata/config-editor/duplicates.env");
    let got: Vec<(String, String)> = parse_fixture(bytes)
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        got,
        tuples(&[
            ("DUP", "first"),
            ("DUP", "second"),
            ("DUP", "third"),
            ("SOLO", "only")
        ])
    );

    // dotenvy::Iter yields every occurrence; first-wins is Iter::load()'s own
    // "only set if not already set" gate (iter.rs:34). Reproduced here
    // without touching the real process environment.
    let mut effective: HashMap<String, String> = HashMap::new();
    for (k, v) in &got {
        effective.entry(k.clone()).or_insert_with(|| v.clone());
    }
    assert_eq!(effective.get("DUP").map(String::as_str), Some("first"));
}

#[test]
fn fixture_references_are_resolved_by_dotenvy_itself() {
    // Documents dotenvy's real substitution behaviour, which the browser
    // editor deliberately does not reproduce (see "Handling of ${VAR}
    // references" in the spec) because it cannot see the target server's
    // process environment and dotenvy checks that first. This is regression
    // coverage for that design decision.
    //
    // URL_BARE_UNDERSCORE_QUIRK pins a real dotenvy quirk: unlike `${VAR}`,
    // a bare `$VAR` reference's name is scanned with `char::is_alphanumeric`
    // (parse.rs), which does not include `_`. So `$ALMONDHOST_SUFFIX` reads
    // as the variable `ALMONDHOST` followed by the literal text `_SUFFIX`,
    // not a variable named `ALMONDHOST_SUFFIX`. Confirmed independently by
    // dotenvy's own `variable_without_parenthesis_is_substituted_before_separators`
    // test. The editor's own `$`/`${...}` scanner (parseValue in
    // config-editor.html) mirrors this deliberately: `[a-zA-Z0-9]`, no `_`.
    let bytes = include_bytes!("../testdata/config-editor/references.env");
    let got: Vec<(String, String)> = parse_fixture(bytes)
        .into_iter()
        .map(|r| r.expect("references.env should parse without error"))
        .collect();
    assert_eq!(
        got,
        tuples(&[
            ("ALMONDHOST", "example.com"),
            ("URL_BLOCK", "https://example.com/path"),
            ("URL_BARE", "https://example.com/path"),
            (
                "URL_BARE_UNDERSCORE_QUIRK",
                "https://example.com_SUFFIX/path"
            ),
            ("ESCAPED", "$ALMONDHOST stays literal"),
            ("UNDEFINED_REF", "><"),
        ])
    );
}

#[test]
fn fixture_multiline_value_spans_lines() {
    let bytes = include_bytes!("../testdata/config-editor/multiline.env");
    let got: Vec<(String, String)> = parse_fixture(bytes)
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        got,
        tuples(&[
            ("SINGLE", "value"),
            ("BLOCK", "first line\nsecond line\nthird line"),
            ("AFTER", "done")
        ])
    );
}

#[test]
fn fixture_errors_matches_expected_ok_err_pattern() {
    let bytes = include_bytes!("../testdata/config-editor/errors.env");
    let results = parse_fixture(bytes);
    assert_eq!(results.len(), 5);
    assert_eq!(
        results[0].as_ref().unwrap(),
        &("GOOD1".to_string(), "fine".to_string())
    );
    assert!(
        results[1].is_err(),
        "a line without '=' should be a parse error"
    );
    assert_eq!(
        results[2].as_ref().unwrap(),
        &("GOOD2".to_string(), "alsofine".to_string())
    );
    assert!(
        results[3].is_err(),
        "an unquoted multi-word value should be a parse error"
    );
    assert_eq!(
        results[4].as_ref().unwrap(),
        &("GOOD3".to_string(), "last".to_string())
    );
}
