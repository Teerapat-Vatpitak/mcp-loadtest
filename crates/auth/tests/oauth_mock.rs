//! End-to-end OAuth tests against a deterministic loopback mock server.

use std::sync::{Arc, Mutex};

use mcp_loadtest_auth::{ClientRegistration, EndpointPolicy, OAuthProvider, PreRegisteredClient};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

#[derive(Debug, Clone)]
struct RecordedRequest {
    path: String,
    body: String,
}

async fn spawn_oauth_server() -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let origin = format!("http://127.0.0.1:{}", address.port());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let task_origin = origin.clone();

    tokio::spawn(async move {
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let (path, body) = read_request(&mut stream).await;
            task_requests
                .lock()
                .expect("request lock")
                .push(RecordedRequest {
                    path: path.clone(),
                    body: body.clone(),
                });

            let response_body = match path.as_str() {
                "/.well-known/oauth-protected-resource/mcp" => format!(
                    r#"{{"resource":"{task_origin}/mcp","authorization_servers":["{task_origin}/issuer"],"scopes_supported":["mcp:read"]}}"#
                ),
                "/.well-known/oauth-authorization-server/issuer" => format!(
                    r#"{{"issuer":"{task_origin}/issuer","authorization_endpoint":"{task_origin}/authorize","token_endpoint":"{task_origin}/token","code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"],"scopes_supported":["mcp:read","offline_access"],"grant_types_supported":["authorization_code","refresh_token"],"authorization_response_iss_parameter_supported":true}}"#
                ),
                "/token" if body.contains("grant_type=authorization_code") => {
                    r#"{"access_token":"access-one","token_type":"Bearer","expires_in":0,"refresh_token":"refresh-one","scope":"mcp:read"}"#.to_owned()
                }
                "/token" if body.contains("grant_type=refresh_token") => {
                    r#"{"access_token":"access-two","token_type":"Bearer","expires_in":3600,"refresh_token":"refresh-two","scope":"mcp:read"}"#.to_owned()
                }
                _ => panic!("unexpected request path/body: {path} {body}"),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).await.expect("write");
        }
    });
    (origin, requests)
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, String) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        let count = stream.read(&mut buffer).await.expect("read");
        assert!(count > 0, "connection closed before headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end])
        .expect("headers")
        .to_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
        })
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut buffer).await.expect("read body");
        assert!(count > 0, "connection closed before body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    let path = headers
        .lines()
        .next()
        .expect("request line")
        .split_ascii_whitespace()
        .nth(1)
        .expect("request target")
        .to_owned();
    let body =
        String::from_utf8(bytes[header_end..header_end + content_length].to_vec()).expect("body");
    (path, body)
}

#[tokio::test]
async fn complete_flow_discovers_exchanges_resource_and_rotates_refresh_token() {
    let (origin, requests) = spawn_oauth_server().await;
    let registration =
        ClientRegistration::pre_registered(PreRegisteredClient::new("test-client").unwrap());
    let provider = OAuthProvider::new(EndpointPolicy::loopback_for_tests(), registration).unwrap();
    let resource = Url::parse(&format!("{origin}/mcp")).unwrap();
    let context = provider.discover(resource, None).await.unwrap();
    let scopes = context.initial_scopes(None, true);
    assert!(scopes.contains("mcp:read"));
    assert!(scopes.contains("offline_access"));

    let redirect = Url::parse("http://127.0.0.1:49152/callback").unwrap();
    let pending = provider
        .begin_authorization(&context, redirect.clone(), scopes)
        .unwrap();
    let authorization_query: std::collections::BTreeMap<_, _> = pending
        .authorization_url()
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(
        authorization_query.get("resource"),
        Some(&format!("{origin}/mcp"))
    );
    let mut callback = redirect;
    callback
        .query_pairs_mut()
        .append_pair("code", "opaque-code")
        .append_pair("state", authorization_query.get("state").unwrap())
        .append_pair("iss", &format!("{origin}/issuer"));

    provider
        .complete_authorization(&context, pending, &callback)
        .await
        .unwrap();
    assert!(
        provider
            .authorization_header(&context)
            .await
            .unwrap()
            .is_some()
    );

    let requests = requests.lock().expect("request lock");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        vec![
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-authorization-server/issuer",
            "/token",
            "/token"
        ]
    );
    let exchange = &requests[2].body;
    assert!(exchange.contains("grant_type=authorization_code"));
    assert!(exchange.contains("code_verifier="));
    assert!(exchange.contains("resource=http%3A%2F%2F127.0.0.1"));
    assert!(exchange.contains("client_id=test-client"));
    let refresh = &requests[3].body;
    assert!(refresh.contains("grant_type=refresh_token"));
    assert!(refresh.contains("refresh_token=refresh-one"));
    assert!(refresh.contains("resource="));
}

#[test]
fn production_policy_rejects_plain_http_cimd() {
    let result = ClientRegistration::client_id_metadata(
        Url::parse("http://client.example/metadata.json").unwrap(),
        &EndpointPolicy::strict(),
    );
    assert!(result.is_err());
}
