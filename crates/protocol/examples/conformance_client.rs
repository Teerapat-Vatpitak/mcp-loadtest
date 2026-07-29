//! Adapter for the official MCP client conformance harness.
//!
//! The harness appends its server URL and selects a scenario through
//! `MCP_CONFORMANCE_SCENARIO`. This intentionally covers only the protocol
//! surface mcp-loadtest uses: discovery, tools/list, tools/call, and the
//! 2026 HTTP metadata headers. It is not an OAuth or general MCP SDK client.

use std::sync::Arc;
use std::time::Duration;

use mcp_loadtest_auth::{
    AuthorizationContext, BearerChallenge, ClientRegistration, ClientSecret, DiscoveryClient,
    DynamicClientMetadata, EndpointPolicy, OAuthProvider, PreRegisteredClient, ScopeSet,
    StepUpTracker, TokenEndpointAuthMethod,
};
use mcp_loadtest_core::ProtocolVersion;
use mcp_loadtest_core::config::ServerConfig;
use mcp_loadtest_protocol::transport::HostGuard;
use mcp_loadtest_protocol::transport::http::HttpTransport;
use mcp_loadtest_protocol::{Session, ToolCallRound};
use serde_json::{Value, json};
use url::Url;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("conformance adapter: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .ok_or("official conformance harness did not append a server URL")?;
    let scenario = std::env::var("MCP_CONFORMANCE_SCENARIO")
        .map_err(|_| "MCP_CONFORMANCE_SCENARIO is not set")?;
    let requested =
        std::env::var("MCP_CONFORMANCE_PROTOCOL_VERSION").unwrap_or_else(|_| "2026-07-28".into());
    if requested != "2026-07-28" {
        return Err(format!("adapter only supports 2026-07-28, got `{requested}`").into());
    }

    let parsed = url::Url::parse(&url)?;
    let host = parsed
        .host_str()
        .ok_or("conformance server URL has no host")?
        .to_owned();
    let mut config = ServerConfig::stdio("conformance-adapter".into(), Vec::new());
    config.allowed_hosts = vec![host];
    let guard = HostGuard::from_config(&config);
    if scenario.starts_with("auth/") {
        return run_auth_scenario(&url, &scenario).await;
    }
    let transport = HttpTransport::connect(&url, &guard).await?;
    let mut session = Session::from_transport_stateless(
        transport,
        Duration::from_secs(10),
        ProtocolVersion::V2026_07_28,
    )
    .await?;

    match scenario.as_str() {
        "request-metadata" => {
            let _ = session.list_tools().await?;
        }
        "tools_call" => {
            let tools = session.list_tools().await?;
            let tool = tools
                .iter()
                .find(|tool| tool.name == "add_numbers")
                .ok_or("tools_call fixture did not advertise add_numbers")?;
            let _ = session
                .call_tool(&tool.name, &json!({"a": 20, "b": 22}))
                .await?;
        }
        "http-standard-headers" => {
            let tools = session.list_tools().await?;
            let tool = tools
                .first()
                .ok_or("standard-header fixture advertised no tools")?;
            let _ = session.call_tool(&tool.name, &json!({})).await?;
        }
        "http-custom-headers" => {
            let tools = session.list_tools().await?;
            require_tool(&tools, "test_custom_headers")?;
            require_tool(&tools, "test_custom_headers_null")?;
            let args = json!({
                "region": "us-west1",
                "priority": 42,
                "verbose": false,
                "debug": true,
                "empty_val": "",
                "method_val": "custom method",
                "float_val": 3.5,
                "non_ascii_val": "Hello, 世界",
                "whitespace_val": " padded ",
                "leading_space_val": " leading",
                "trailing_space_val": "trailing ",
                "internal_space_val": "hello world",
                "control_char_val": "line1\nline2",
                "crlf_val": "line1\r\nline2",
                "tab_val": "\tvalue",
                "query": "select 1"
            });
            let _ = session.call_tool("test_custom_headers", &args).await?;
            let _ = session
                .call_tool(
                    "test_custom_headers_null",
                    &json!({
                        "region": "us-west1",
                        "priority": 1,
                        "verbose": Value::Null,
                        "query": "select 1"
                    }),
                )
                .await?;
        }
        "http-invalid-tool-headers" => {
            let tools = session.list_tools().await?;
            if tools.len() != 1 || tools[0].name != "valid_tool" {
                return Err(format!(
                    "invalid x-mcp-header tools were not filtered: {:?}",
                    tools.iter().map(|tool| &tool.name).collect::<Vec<_>>()
                )
                .into());
            }
            let _ = session
                .call_tool("valid_tool", &json!({"region": "us-west1"}))
                .await?;
        }
        "sep-2322-client-request-state" => {
            let _ = session.list_tools().await?;
            let accepted = json!({
                "confirm": {
                    "resultType": "complete",
                    "action": "accept",
                    "content": {"confirmed": true}
                }
            });

            let echo = session
                .call_tool_round("test_mrtr_echo_state", &json!({}), None, None)
                .await?;
            let ToolCallRound::InputRequired(echo) = echo else {
                return Err("echo-state tool did not request input".into());
            };
            let _ = session
                .call_tool_round("test_mrtr_unrelated", &json!({}), None, None)
                .await?;
            let _ = session
                .call_tool_round(
                    "test_mrtr_echo_state",
                    &json!({}),
                    echo.request_state.as_deref(),
                    Some(&accepted),
                )
                .await?;

            let no_state = session
                .call_tool_round("test_mrtr_no_state", &json!({}), None, None)
                .await?;
            let ToolCallRound::InputRequired(no_state) = no_state else {
                return Err("no-state tool did not request input".into());
            };
            if no_state.request_state.is_some() {
                return Err("no-state fixture unexpectedly returned requestState".into());
            }
            let _ = session
                .call_tool_round("test_mrtr_no_state", &json!({}), None, Some(&accepted))
                .await?;
            let _ = session
                .call_tool_round("test_mrtr_no_result_type", &json!({}), None, None)
                .await?;
        }
        "json-schema-ref-no-deref" => {
            let _ = session.list_tools().await?;
        }
        other => return Err(format!("unsupported conformance scenario `{other}`").into()),
    }

    session.shutdown().await?;
    Ok(())
}

#[derive(Clone)]
enum RegistrationStrategy {
    Dynamic,
    Static(ClientRegistration),
}

struct ConformanceAuthClient {
    http: reqwest::Client,
    policy: EndpointPolicy,
    resource: Url,
    registration: RegistrationStrategy,
    provider: Arc<OAuthProvider>,
    context: AuthorizationContext,
    scopes: ScopeSet,
    step_up: StepUpTracker,
}

async fn run_auth_scenario(
    server_url: &str,
    scenario: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let resource = Url::parse(server_url)?;
    let policy = EndpointPolicy::loopback_for_tests();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let challenge = initial_auth_challenge(&http, &resource).await?;
    let discovery = DiscoveryClient::new(policy.clone())?;
    let context = discovery
        .discover(resource.clone(), Some(&challenge))
        .await?;
    let registration = registration_strategy(scenario, &context, &policy).await?;
    let resolved_registration = resolve_registration(&registration, &context, &policy).await?;
    let provider = Arc::new(OAuthProvider::new(policy.clone(), resolved_registration)?);
    let scopes = context.initial_scopes(Some(&challenge), true);
    authorize_interactively(&http, &provider, &context, scopes.clone()).await?;

    let mut client = ConformanceAuthClient {
        http,
        policy,
        resource,
        registration,
        provider,
        context,
        scopes,
        step_up: StepUpTracker::default(),
    };
    let discover = final_request(1, "server/discover", json!({}));
    let _ = client.send(discover, "server/discover", None).await?;
    let list = final_request(2, "tools/list", json!({}));
    let _ = client.send(list, "tools/list", None).await?;
    let call = final_request(3, "tools/call", json!({"name":"test-tool","arguments":{}}));
    let _ = client.send(call, "tools/call", Some("test-tool")).await?;
    Ok(())
}

impl ConformanceAuthClient {
    async fn send(
        &mut self,
        body: Value,
        method: &str,
        name: Option<&str>,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        loop {
            let mut request = self
                .http
                .post(self.resource.clone())
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", method)
                .json(&body);
            if let Some(name) = name {
                request = request.header("Mcp-Name", name);
            }
            let header = self
                .provider
                .authorization_header(&self.context)
                .await?
                .ok_or("OAuth provider has no access token")?;
            let response = header.apply(request).send().await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED
                || response.status() == reqwest::StatusCode::FORBIDDEN
            {
                let challenge = parse_challenge(response.headers())?;
                self.reauthorize(challenge).await?;
                continue;
            }
            if !response.status().is_success() {
                return Err(format!("protected MCP request returned {}", response.status()).into());
            }
            let value: Value = response.json().await?;
            if let Some(error) = value.get("error") {
                return Err(format!(
                    "protected MCP JSON-RPC request failed with code {}",
                    error
                        .get("code")
                        .and_then(Value::as_i64)
                        .unwrap_or_default()
                )
                .into());
            }
            return Ok(value);
        }
    }

    async fn reauthorize(
        &mut self,
        challenge: BearerChallenge,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let discovery = DiscoveryClient::new(self.policy.clone())?;
        let context = discovery
            .discover(self.resource.clone(), Some(&challenge))
            .await?;
        let challenged = context.initial_scopes(Some(&challenge), true);
        let scopes = self.step_up.next(&self.scopes, &challenged)?;
        if context.exact_issuer() != self.context.exact_issuer() {
            let registration =
                resolve_registration(&self.registration, &context, &self.policy).await?;
            self.provider = Arc::new(OAuthProvider::new(self.policy.clone(), registration)?);
        }
        authorize_interactively(&self.http, &self.provider, &context, scopes.clone()).await?;
        self.context = context;
        self.scopes = scopes;
        Ok(())
    }
}

async fn initial_auth_challenge(
    http: &reqwest::Client,
    resource: &Url,
) -> Result<BearerChallenge, Box<dyn std::error::Error>> {
    let body = final_request(1, "server/discover", json!({}));
    let response = http
        .post(resource.clone())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "server/discover")
        .json(&body)
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::UNAUTHORIZED
        && response.status() != reqwest::StatusCode::FORBIDDEN
    {
        return Err(format!(
            "auth scenario did not begin with an authorization challenge ({})",
            response.status()
        )
        .into());
    }
    parse_challenge(response.headers())
}

fn parse_challenge(
    headers: &reqwest::header::HeaderMap,
) -> Result<BearerChallenge, Box<dyn std::error::Error>> {
    let values: Vec<String> = headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .map(|value| value.to_str().map(str::to_owned))
        .collect::<Result<_, _>>()?;
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    BearerChallenge::parse(&refs)?
        .ok_or_else(|| "authorization response omitted a Bearer challenge".into())
}

async fn registration_strategy(
    scenario: &str,
    _context: &AuthorizationContext,
    policy: &EndpointPolicy,
) -> Result<RegistrationStrategy, Box<dyn std::error::Error>> {
    if scenario == "auth/basic-cimd" {
        let registration = ClientRegistration::client_id_metadata(
            Url::parse("https://conformance-test.local/client-metadata.json")?,
            policy,
        )?;
        return Ok(RegistrationStrategy::Static(registration));
    }
    if scenario == "auth/pre-registration" {
        let raw = std::env::var("MCP_CONFORMANCE_CONTEXT")?;
        let context: Value = serde_json::from_str(&raw)?;
        let client_id = context["client_id"]
            .as_str()
            .ok_or("pre-registration context omitted client_id")?;
        let secret = context["client_secret"]
            .as_str()
            .ok_or("pre-registration context omitted client_secret")?
            .to_owned();
        let source = ClientSecret::from_resolver(move || Ok(secret.clone()));
        let client = PreRegisteredClient::new(client_id)?
            .with_client_secret(source)
            .with_token_endpoint_auth_method(TokenEndpointAuthMethod::ClientSecretBasic);
        return Ok(RegistrationStrategy::Static(
            ClientRegistration::pre_registered(client),
        ));
    }
    Ok(RegistrationStrategy::Dynamic)
}

async fn resolve_registration(
    strategy: &RegistrationStrategy,
    context: &AuthorizationContext,
    policy: &EndpointPolicy,
) -> Result<ClientRegistration, Box<dyn std::error::Error>> {
    match strategy {
        RegistrationStrategy::Static(registration) => Ok(registration.clone()),
        RegistrationStrategy::Dynamic => {
            let redirect = Url::parse("http://localhost:3000/callback")?;
            let metadata =
                DynamicClientMetadata::authorization_code("test-auth-client", &redirect)?;
            Ok(OAuthProvider::dynamic_register(
                policy.clone(),
                &context.authorization_server,
                &metadata,
            )
            .await?)
        }
    }
}

async fn authorize_interactively(
    http: &reqwest::Client,
    provider: &OAuthProvider,
    context: &AuthorizationContext,
    scopes: ScopeSet,
) -> Result<(), Box<dyn std::error::Error>> {
    let redirect = Url::parse("http://localhost:3000/callback")?;
    let pending = provider.begin_authorization(context, redirect, scopes)?;
    let response = http.get(pending.authorization_url().clone()).send().await?;
    if !response.status().is_redirection() {
        return Err(format!(
            "authorization endpoint did not redirect ({})",
            response.status()
        )
        .into());
    }
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .ok_or("authorization redirect omitted Location")?
        .to_str()?;
    let callback = pending.authorization_url().join(location)?;
    let _ = provider
        .complete_authorization(context, pending, &callback)
        .await?;
    Ok(())
}

fn final_request(id: u64, method: &str, params: Value) -> Value {
    let mut params = params.as_object().cloned().unwrap_or_default();
    params.insert(
        "_meta".to_owned(),
        json!({
            "io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientInfo":{
                "name":"mcp-loadtest-conformance","version":env!("CARGO_PKG_VERSION")
            },
            "io.modelcontextprotocol/clientCapabilities":{}
        }),
    );
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

fn require_tool(
    tools: &[mcp_loadtest_protocol::mcp::Tool],
    expected: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if tools.iter().any(|tool| tool.name == expected) {
        Ok(())
    } else {
        Err(format!("fixture did not advertise `{expected}`").into())
    }
}
