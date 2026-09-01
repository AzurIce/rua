//! Integration test for `summarize` against a mock non-streaming endpoint.

use rua_core::config::ProviderConfig;
use rua_engine::Engine;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn engine_for(server: &MockServer) -> Engine {
    let config = ProviderConfig {
        kind: "deepseek".to_string(),
        api_key: "test-key".to_string(),
        base_url: server.uri(),
        model: "deepseek-v4-pro".to_string(),
        additional_params: serde_json::Map::new(),
    };
    Engine::new(
        &[(rua_core::config::DEFAULT_PROVIDER.to_string(), config)],
        std::env::current_dir().unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn summarize_returns_body_text() {
    let server = MockServer::start().await;
    let response = serde_json::json!({
        "id": "cmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "deepseek-v4-pro",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "- fact one\n- decision two"},
            "logprobs": null,
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 42,
            "completion_tokens": 7,
            "prompt_cache_hit_tokens": 0,
            "prompt_cache_miss_tokens": 42,
            "total_tokens": 49
        }
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        // The material must be embedded in the prompt.
        .and(body_string_contains("raw material here"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(response.to_string(), "application/json"),
        )
        .mount(&server)
        .await;

    let engine = engine_for(&server);
    let body = engine
        .summarize("deepseek-v4-pro", "raw material here")
        .await
        .unwrap();
    assert_eq!(body, "- fact one\n- decision two");
}
