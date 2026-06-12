use gfs_console_remote::{auth_from_env, ConsoleAuth, ConsoleClient};
use gfs_domain::model::config::RemoteConfig;
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_remote() -> RemoteConfig {
    RemoteConfig {
        console_url: "http://localhost".into(),
        deployment_id: Some("dep-1".into()),
        node_id: "node-1".into(),
        database_id: "cp-db-1".into(),
        project: "default".into(),
    }
}

#[tokio::test]
async fn deploy_commit_and_log_use_bearer_and_deployment_routes() {
    let server = MockServer::start().await;
    let token = "test-jwt";

    Mock::given(method("POST"))
        .and(path("/api/engine/deployments"))
        .and(header("Authorization", format!("Bearer {token}")))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "deployment": { "id": "dep-1" },
            "engine": { "status": "INIT" }
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/engine/deployments/dep-1/commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "commit": "abc123" })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/engine/deployments/dep-1/log"))
        .and(query_param("n", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{ "hash": "abc123" }])))
        .mount(&server)
        .await;

    let client = ConsoleClient::new(
        server.uri(),
        ConsoleAuth {
            access_token: token.into(),
        },
    )
    .unwrap();

    let deploy = client
        .deploy_database("node-1", "postgres", "17", None, "default")
        .await
        .unwrap();
    assert_eq!(deploy.deployment["id"], "dep-1");

    let remote = test_remote();
    let commit = client.commit(&remote, "msg").await.unwrap();
    assert_eq!(commit["commit"], "abc123");

    let log = client.log(&remote, 5).await.unwrap();
    assert!(log.is_array());
}

#[tokio::test]
async fn auth_from_env_prefers_env_var() {
    unsafe {
        std::env::set_var("GUEPARD_ACCESS_TOKEN", "from-env");
    }
    let auth = auth_from_env().unwrap();
    assert_eq!(auth.access_token, "from-env");
    unsafe {
        std::env::remove_var("GUEPARD_ACCESS_TOKEN");
    }
}
