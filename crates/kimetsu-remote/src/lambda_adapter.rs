//! Lambda event adaptation reuses the ordinary authenticated HTTP router.
use axum::{Router, body::Body};
use tower::ServiceExt;

pub async fn handle(
    router: Router,
    request: lambda_http::Request,
) -> Result<lambda_http::Response<lambda_http::Body>, lambda_http::Error> {
    let (parts, body) = request.into_parts();
    let body = match body {
        lambda_http::Body::Empty => Body::empty(),
        lambda_http::Body::Text(value) => Body::from(value),
        lambda_http::Body::Binary(value) => Body::from(value),
        _ => return Err("unsupported Lambda request body".into()),
    };
    let response = router
        .oneshot(axum::http::Request::from_parts(parts, body))
        .await?;
    let (parts, body) = response.into_parts();
    // Leave room for API Gateway's JSON envelope and base64/escaping overhead.
    let bytes = axum::body::to_bytes(body, 2 * 1024 * 1024).await?;
    let body = match String::from_utf8(bytes.to_vec()) {
        Ok(text) => lambda_http::Body::Text(text),
        Err(_) => lambda_http::Body::Binary(bytes.to_vec()),
    };
    Ok(lambda_http::Response::from_parts(parts, body))
}

pub async fn run(router: Router) -> Result<(), lambda_http::Error> {
    lambda_http::run(lambda_http::service_fn(move |request| {
        handle(router.clone(), request)
    }))
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::build_router, auth::AuthConfig, state::AppState};

    #[tokio::test]
    async fn lambda_preserves_router_health_and_auth_responses() {
        let root = tempfile::tempdir().unwrap();
        let auth = AuthConfig {
            global: vec!["fixture-token".into()],
            ..Default::default()
        };
        let router = build_router(AppState::new(root.path().to_path_buf(), auth));
        let request = axum::http::Request::builder()
            .uri("/healthz")
            .body(lambda_http::Body::Empty)
            .unwrap();
        let result = handle(router.clone(), request).await.unwrap();
        assert_eq!(result.status(), 200);
        assert_eq!(result.body(), &lambda_http::Body::Text("ok".into()));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp/demo")
            .header("content-type", "application/json")
            .body(lambda_http::Body::Binary(
                br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_vec(),
            ))
            .unwrap();
        let result = handle(router, request).await.unwrap();
        assert_eq!(result.status(), 401);
        let lambda_http::Body::Text(body) = result.body() else {
            panic!("JSON response must be text")
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap()["error"],
            "unauthorized"
        );
    }
}
