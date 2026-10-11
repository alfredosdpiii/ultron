//! Literal expectations for the JavaScript semantics and helpers the ported subcommands print with (values checked
//! against Node: `toFixed`, `Date#getHours` under TZ, `String.prototype.replace`, `JSON.stringify(v, null, "\t")`),
//! and the ported `session-stats.test.ts` cases for cell APIs and steers.

use cli::js::{base64, base64_decode, replace_first, to_fixed};
use cli::localtime::{Fields, Zone};
use cli::usage::stats::{cell_apis, classify_nudge};
use serde_json::json;

#[test]
fn cli_js_to_fixed_rounds_exact_ties_up() {
    // (0.125).toFixed(2) === "0.13"; (2.5).toFixed(0) === "3"; (1.005).toFixed(2) === "1.00" (binary below the tie)
    assert_eq!(to_fixed(0.125, 2), "0.13");
    assert_eq!(to_fixed(2.5, 0), "3");
    assert_eq!(to_fixed(1.005, 2), "1.00");
    assert_eq!(to_fixed(9.995, 2), "9.99");
    assert_eq!(to_fixed(0.0625, 3), "0.063");
    assert_eq!(to_fixed(99.95, 1), "100.0");
    assert_eq!(to_fixed(-0.125, 2), "-0.13");
}

#[test]
fn cli_js_replace_first_expands_dollar_patterns() {
    // "a{{X}}b".replace("{{X}}", "[$&|$`|$'|$$|$1]") === "a[{{X}}|a|b|$|$1]b"
    assert_eq!(
        replace_first("a{{X}}b{{X}}", "{{X}}", "[$&|$`|$'|$$|$1]"),
        "a[{{X}}|a|b{{X}}|$|$1]b{{X}}"
    );
    assert_eq!(replace_first("abc", "z", "$&"), "abc");
}

#[test]
fn cli_js_base64_round_trips() {
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64("ü😀".as_bytes()), "w7zwn5iA");
    assert_eq!(base64_decode("w7zwn5iA").unwrap(), "ü😀".as_bytes());
}

#[test]
fn cli_localtime_posix_rules_match_node() {
    let ny = Zone::from_posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
    let at = |ms: f64| ny.fields(ms);
    // 2026-03-08T06:59:59Z is 01:59 EST; 07:00Z is 03:00 EDT.
    assert_eq!(
        at(1_772_953_199_000.0),
        Fields {
            year: 2026,
            month: 3,
            day: 8,
            hour: 1,
            minute: 59
        }
    );
    assert_eq!(
        at(1_772_953_200_000.0),
        Fields {
            year: 2026,
            month: 3,
            day: 8,
            hour: 3,
            minute: 0
        }
    );
    // 2026-11-01T06:00:00Z is 01:00 EST (fall back at 06:00Z); 05:59Z is 01:59 EDT.
    assert_eq!(
        at(1_793_512_740_000.0),
        Fields {
            year: 2026,
            month: 11,
            day: 1,
            hour: 1,
            minute: 59
        }
    );
    assert_eq!(
        at(1_793_512_800_000.0),
        Fields {
            year: 2026,
            month: 11,
            day: 1,
            hour: 1,
            minute: 0
        }
    );
    // Southern hemisphere: Sydney, AEST-10AEDT,M10.1.0,M4.1.0/3. 2026-01-01T00:00Z is 11:00 AEDT.
    let sydney = Zone::from_posix("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
    assert_eq!(sydney.fields(1_767_225_600_000.0).hour, 11);
    assert_eq!(sydney.fields(1_782_864_000_000.0).hour, 10);
    // A fixed offset with a quoted name, and a fractional one.
    assert_eq!(
        Zone::from_posix("<+0530>-5:30").unwrap().fields(0.0),
        Fields {
            year: 1970,
            month: 1,
            day: 1,
            hour: 5,
            minute: 30
        }
    );
    assert_eq!(
        Zone::utc().fields(-1.0),
        Fields {
            year: 1969,
            month: 12,
            day: 31,
            hour: 23,
            minute: 59
        }
    );
}

#[test]
fn cli_localtime_reads_zoneinfo_files() {
    let path = std::path::Path::new("/usr/share/zoneinfo/America/New_York");
    if !path.exists() {
        return;
    }
    let zone = Zone::from_env(Some("America/New_York"));
    assert_eq!(zone.offset(1_772_953_200_000.0), -4 * 3600);
    assert_eq!(zone.offset(1_772_953_199_000.0), -5 * 3600);
    // 1990 is inside the transition table; 2090 relies on the footer rule.
    assert_eq!(zone.offset(647_100_000_000.0), -4 * 3600);
    assert_eq!(zone.offset(3_802_982_400_000.0), -4 * 3600);
    assert_eq!(zone.offset(3_787_862_400_000.0), -5 * 3600);
    assert_eq!(Zone::from_env(Some("Nowhere/Invalid")).offset(0.0), 0);
    assert_eq!(Zone::from_env(Some(":UTC")).offset(0.0), 0);
}

#[test]
fn cli_usage_cell_apis_match_session_stats_test() {
    assert_eq!(
        cell_apis("out = await bash(\"ls\")\ntext = await read ( \"a\" )\nobj.read(1)\nx.bash(2)"),
        ["bash", "read"]
    );
    assert_eq!(
        cell_apis("await rlm.map(f, xs)\nmyrlm.infer(1)\nawait agents.invoke(x)"),
        ["rlm.map", "agents.*"]
    );
    assert_eq!(cell_apis("def f():\n    return view_image('a.png')"), ["view_image"]);
    assert_eq!(cell_apis("bashful()\nreader(x)\nwrite_file()"), Vec::<&str>::new());
    assert_eq!(
        cell_apis("await background.start(...)\nworkflows.run(w)\nmcp.call(t)\ntools.call(t)\nrlm.merge([w])"),
        [
            "rlm.merge",
            "mcp.call",
            "tools.call",
            "workflows.run",
            "background.start",
        ]
    );
}

#[test]
fn cli_usage_classify_nudge_matches_session_stats_test() {
    assert_eq!(
        classify_nudge("[Ultron] 5 tool rounds in a row succeeded. Save a skill."),
        Some("skill")
    );
    assert_eq!(
        classify_nudge("[Ultron] 1 subagent or task you started is still running."),
        Some("wait")
    );
    assert_eq!(
        classify_nudge("[Ultron] 3 subagents or tasks you started are still running"),
        Some("wait")
    );
    assert_eq!(
        classify_nudge("[Ultron] You have now used 12 rounds of tool calls"),
        Some("toolRounds")
    );
    assert_eq!(classify_nudge("[Ultron] You have used 12 rounds of tool callsx"), None);
    assert_eq!(classify_nudge("hello"), None);
    assert_eq!(classify_nudge(" [Ultron] 5 tool rounds in a row succeeded"), None);
}

#[test]
fn cli_mcp_edit_keeps_indentation_and_js_key_order() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nested/mcp.json");
    let config = |c: &str| json!({ "command": c }).as_object().unwrap().clone();
    assert!(!mcp::edit::add_server_config(&path, "a", config("x")).unwrap());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\n  \"mcpServers\": {\n    \"a\": {\n      \"command\": \"x\"\n    }\n  }\n}\n"
    );
    std::fs::write(&path, "{\n\t\"1\": 1.5e-7, \"mcpServers\": {\"a\": {}}, \"0\": []}").unwrap();
    assert!(mcp::edit::add_server_config(&path, "a", config("y")).unwrap());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\n\t\"0\": [],\n\t\"1\": 1.5e-7,\n\t\"mcpServers\": {\n\t\t\"a\": {\n\t\t\t\"command\": \"y\"\n\t\t}\n\t}\n}\n"
    );
    assert!(mcp::edit::remove_server_config(&path, "a").unwrap());
    assert!(!mcp::edit::remove_server_config(&path, "a").unwrap());
    assert!(!mcp::edit::remove_server_config(&tmp.path().join("none.json"), "a").unwrap());
    std::fs::write(&path, "[]").unwrap();
    assert!(mcp::edit::add_server_config(&path, "a", config("y")).is_err());
}

#[test]
fn cli_usage_format_tokens_and_elapsed_match_report_text() {
    use cli::usage::text::{format_elapsed, format_tokens};
    let tokens: Vec<String> = [999.0, 999_949.0, 999_950.0, 99_949_999.0, 99_950_000.0, 1234.0]
        .into_iter()
        .map(format_tokens)
        .collect();
    assert_eq!(tokens, ["999", "999.9k", "1.00M", "99.95M", "100.0M", "1.2k"]);
    let elapsed: Vec<String> = [499.0, 500.0, 59_499.0, 59_500.0, 3_599_499.0, 3_600_000.0, 7_322_000.0]
        .into_iter()
        .map(format_elapsed)
        .collect();
    assert_eq!(elapsed, ["0s", "1s", "59s", "1m00s", "59m59s", "1h00m", "2h02m"]);
}

#[test]
fn cli_export_derived_colors_follow_the_template_formula() {
    use cli::export_html::derive_export_colors;
    // deriveExportColors("#343541") (dark): 0.7 and 0.85 brightness, info +20/+15.
    assert_eq!(
        derive_export_colors("#343541"),
        ["rgb(36, 37, 46)", "rgb(44, 45, 55)", "rgb(72, 68, 65)"]
    );
    // A light base keeps itself as the card color.
    assert_eq!(
        derive_export_colors("#ffffff"),
        ["rgb(245, 245, 245)", "#ffffff", "rgb(255, 255, 235)"]
    );
    assert_eq!(
        derive_export_colors("rgb( 10 , 20 , 30 )"),
        ["rgb(7, 14, 21)", "rgb(9, 17, 26)", "rgb(30, 35, 30)"]
    );
    assert_eq!(
        derive_export_colors("nope"),
        ["rgb(24, 24, 30)", "rgb(30, 30, 36)", "rgb(60, 55, 40)"]
    );
}

#[test]
fn cli_usage_log_usage_matches_session_log() {
    use cli::usage::log::log_usage;
    // logUsage({ input: 3, output: 4, cacheRead: -2 }): totals fall back to the sum of the positive parts.
    let usage = log_usage(Some(&json!({"input": 3, "output": 4, "cacheRead": -2}))).unwrap();
    assert_eq!((usage.total_tokens, usage.cache_read, usage.cost), (7.0, 0.0, None));
    let usage = log_usage(Some(&json!({"input": 1, "totalTokens": 10, "cost": {"total": 0.5}}))).unwrap();
    assert_eq!((usage.total_tokens, usage.cost), (10.0, Some(0.5)));
    let usage = log_usage(Some(&json!({"input": 2, "totalTokens": 0, "cost": {"total": -1}}))).unwrap();
    assert_eq!((usage.total_tokens, usage.cost), (2.0, None));
    assert!(log_usage(Some(&json!([1]))).is_none());
    assert!(log_usage(None).is_none());
}
