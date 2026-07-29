//! Editor-facing configuration-schema contract tests.

use std::collections::BTreeSet;

use mcp_loadtest_core::config::{CONFIG_SCHEMA_JSON, Config, config_schema, config_schema_pretty};
use serde_json::Value;

#[test]
fn embedded_schema_is_valid_json_and_renders_deterministically() {
    let parsed: Value =
        serde_json::from_str(CONFIG_SCHEMA_JSON).expect("checked-in config schema is valid JSON");
    assert_eq!(config_schema(), parsed);

    let mut expected = serde_json::to_string_pretty(&parsed).expect("schema serializes");
    expected.push('\n');
    assert_eq!(config_schema_pretty(), expected);
    assert_eq!(
        parsed["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
}

#[test]
fn schema_advertises_every_runtime_scenario_and_v02_block() {
    let schema = config_schema();
    let definitions = schema["$defs"].as_object().expect("schema has definitions");
    let names = definitions["scenario"]["oneOf"]
        .as_array()
        .expect("scenario union")
        .iter()
        .map(|branch| {
            let reference = branch["$ref"].as_str().expect("scenario reference");
            let definition = reference
                .strip_prefix("#/$defs/")
                .expect("local definition reference");
            definitions[definition]["properties"]["type"]["const"]
                .as_str()
                .expect("scenario discriminator")
        })
        .collect::<BTreeSet<_>>();

    assert_eq!(
        names,
        BTreeSet::from([
            "cold_start",
            "deadlock_probe",
            "fuzzer",
            "pattern",
            "race_check",
            "ramp",
            "soak",
            "spike",
            "sustained",
            "version_matrix",
        ])
    );
    assert_eq!(
        definitions["distributed"]["properties"]["agents"]["minItems"],
        2
    );
    assert_eq!(
        definitions["auth"]["properties"]["max_step_up_retries"]["maximum"],
        3
    );
    assert_eq!(
        definitions["otlpOutput"]["properties"]["max_attempts"]["maximum"],
        10
    );
    assert_eq!(
        definitions["historyOutput"]["properties"]["min_samples"]["default"],
        3
    );
}

#[test]
fn runtime_accepts_schema_documented_v02_configuration() {
    let config = Config::from_toml_str(
        r#"
[server]
transport = "http"
url = "https://mcp.example.com/mcp"
allowed_hosts = ["mcp.example.com"]
protocol_version = "2025-11-25"

[server.auth]
type = "oauth"
flow = "client_credentials"
registration = "pre_registered"
client_id = "loadtest"
client_secret_env = "MCP_CLIENT_SECRET"
scopes = ["mcp:tools"]

[scenario]
type = "sustained"
concurrent = 2
duration = "5s"
tool = "echo"
args = { message = "hello" }

[output]
formats = ["junit", "prometheus"]

[output.otlp]
endpoint = "https://otel.example.com/v1/metrics"
timeout = "10s"
max_attempts = 3
allowed_hosts = ["otel.example.com"]

[output.otlp.headers_from_env]
Authorization = "OTLP_AUTHORIZATION"

[output.history]
series = "main-sustained"
directory = "./runs/history"
window = 10
min_samples = 3
require_history = false
max_p99_regression_pct = 10.0
max_error_rate_regression_pp = 0.5
max_rps_drop_pct = 10.0
deadlock_zero_tolerance = true

[distributed]
require_all_agents = true
connect_timeout = "20s"
ready_timeout = "60s"
heartbeat_timeout = "15s"
start_lead = "1s"

[[distributed.agents]]
name = "loadgen-a"
ssh_host = "runner@loadgen-a.example.com"

[[distributed.agents]]
name = "loadgen-b"
ssh_host = "runner@loadgen-b.example.com"
"#,
    )
    .expect("schema-documented v0.2 config must parse and validate");

    assert_eq!(config.output.formats, ["junit", "prometheus"]);
    assert_eq!(
        config.output.history.as_ref().expect("history").series,
        "main-sustained"
    );
    assert_eq!(
        config
            .distributed
            .as_ref()
            .expect("distributed")
            .agents
            .len(),
        2
    );
}
