#![doc = include_str!("../README.md")]

mod auth;
mod config;
mod runtime;

pub use auth::BearerToken;
pub use config::{Config, ConfigError, ConfigErrorKind, RequestLimits, TransportSecurity};
pub use runtime::{
    Error, ErrorKind, LIVENESS_PATH, READINESS_PATH, UNAUTHORIZED_BODY, protected_router, serve,
};

#[cfg(test)]
mod tests {
    use super::Config as ServerConfig;
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::StatusCode,
        routing::{get, post},
    };
    use tower::ServiceExt;

    fn loopback_config(token: Option<BearerToken>, limits: RequestLimits) -> ServerConfig {
        ServerConfig::new(
            "127.0.0.1:0"
                .parse()
                .expect("test loopback socket address must parse"),
            TransportSecurity::PlaintextLoopback,
            token,
            limits,
        )
    }

    #[tokio::test]
    async fn protected_routes_require_the_exact_bearer_token() {
        let token = BearerToken::new("correct-secret").expect("test token must be valid");
        let router = protected_router(
            Router::new().route("/private", get(|| async { "reached" })),
            &loopback_config(Some(token), RequestLimits::default()),
        )
        .expect("test router configuration must be valid");

        let missing = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            missing
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        let missing_body = to_bytes(missing.into_body(), 1024)
            .await
            .expect("authentication body must be readable");
        assert_eq!(missing_body.as_ref(), UNAUTHORIZED_BODY.as_bytes());

        let wrong = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .header(axum::http::header::AUTHORIZATION, "Bearer wrong-secret")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let wrong_body = to_bytes(wrong.into_body(), 1024)
            .await
            .expect("authentication body must be readable");
        assert_eq!(
            wrong_body.as_ref(),
            UNAUTHORIZED_BODY.as_bytes(),
            "the rejection must not say which part of the credential was wrong"
        );

        let accepted = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .header(axum::http::header::AUTHORIZATION, "Bearer correct-secret")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(accepted.status(), StatusCode::OK);
        let body = to_bytes(accepted.into_body(), 1024)
            .await
            .expect("response body must be readable");
        assert_eq!(body.as_ref(), b"reached");
    }

    #[test]
    fn health_paths_are_the_documented_contract() {
        // The application owns the handlers (payment-api serves and documents
        // both routes); this crate owns only the two paths, so a change here
        // is a change to the published HTTP contract.
        assert_eq!(LIVENESS_PATH, "/health/live");
        assert_eq!(READINESS_PATH, "/health/ready");
        assert_ne!(LIVENESS_PATH, READINESS_PATH);
    }

    #[tokio::test]
    async fn authentication_precedes_body_limit_rejection() {
        let config = loopback_config(
            Some(BearerToken::new("fixture-bearer").expect("valid fixture")),
            RequestLimits::new(4).expect("valid limit"),
        );
        let router = protected_router(
            Router::new().route("/echo", post(|body: String| async move { body })),
            &config,
        )
        .expect("valid router");
        for (authorization, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("Bearer wrong-fixture"), StatusCode::UNAUTHORIZED),
            (Some("Bearer fixture-bearer"), StatusCode::PAYLOAD_TOO_LARGE),
        ] {
            let mut request = axum::http::Request::builder().method("POST").uri("/echo");
            if let Some(authorization) = authorization {
                request = request.header(axum::http::header::AUTHORIZATION, authorization);
            }
            let response = router
                .clone()
                .oneshot(request.body(Body::from("12345")).expect("valid request"))
                .await
                .expect("response");
            assert_eq!(response.status(), expected);
            if expected == StatusCode::UNAUTHORIZED {
                assert_eq!(
                    response.headers()[axum::http::header::WWW_AUTHENTICATE],
                    "Bearer"
                );
                assert_eq!(
                    to_bytes(response.into_body(), 1024)
                        .await
                        .expect("body")
                        .as_ref(),
                    UNAUTHORIZED_BODY.as_bytes()
                );
            }
        }
    }

    #[test]
    fn protected_router_validates_transport_before_authentication() {
        for (transport, expected) in [
            (
                TransportSecurity::PlaintextLoopback,
                ConfigErrorKind::InsecureNonLoopbackBind,
            ),
            (
                TransportSecurity::TlsTerminatedUpstream,
                ConfigErrorKind::MissingBearerToken,
            ),
        ] {
            let config = ServerConfig::new(
                "0.0.0.0:8443".parse().expect("address"),
                transport,
                None,
                RequestLimits::default(),
            );
            let expected_error = config.validate().expect_err("invalid configuration");
            let error = protected_router(Router::new(), &config).expect_err("invalid router");
            assert_eq!(error.kind, expected);
            assert_eq!(error, expected_error);
        }
    }

    #[tokio::test]
    async fn configured_body_limit_rejects_large_requests() {
        let limits = RequestLimits::new(4).expect("test limits must be valid");
        let token = BearerToken::new("configured-bearer").expect("test token must be valid");
        let router = protected_router(
            Router::new().route("/echo", post(|body: String| async move { body })),
            &loopback_config(Some(token), limits),
        )
        .expect("test router configuration must be valid");

        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/echo")
                    .header(
                        axum::http::header::AUTHORIZATION,
                        "Bearer configured-bearer",
                    )
                    .header(axum::http::header::CONTENT_TYPE, "text/plain")
                    .body(Body::from("12345"))
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn listeners_require_upstream_tls_and_a_bearer_token() {
        let address = "0.0.0.0:8080"
            .parse()
            .expect("test non-loopback socket address must parse");
        let insecure = ServerConfig::new(
            address,
            TransportSecurity::PlaintextLoopback,
            None,
            RequestLimits::default(),
        );
        assert_eq!(
            insecure
                .validate()
                .expect_err("insecure bind must fail")
                .kind,
            ConfigErrorKind::InsecureNonLoopbackBind
        );

        let unauthenticated = ServerConfig::new(
            address,
            TransportSecurity::TlsTerminatedUpstream,
            None,
            RequestLimits::default(),
        );
        assert_eq!(
            unauthenticated
                .validate()
                .expect_err("unauthenticated bind must fail")
                .kind,
            ConfigErrorKind::MissingBearerToken
        );

        let loopback_without_bearer = ServerConfig::new(
            "127.0.0.1:8080"
                .parse()
                .expect("test loopback socket address must parse"),
            TransportSecurity::PlaintextLoopback,
            None,
            RequestLimits::default(),
        );
        assert_eq!(
            loopback_without_bearer
                .validate()
                .expect_err("loopback routes must not fail open")
                .kind,
            ConfigErrorKind::MissingBearerToken
        );

        ServerConfig::new(
            address,
            TransportSecurity::TlsTerminatedUpstream,
            Some(BearerToken::new("declared-bearer").expect("test token must be valid")),
            RequestLimits::default(),
        )
        .validate()
        .expect("upstream TLS plus a bearer token is the one accepted non-loopback listener");
    }

    #[test]
    fn body_limits_are_configurable_and_reject_zero() {
        let limits = RequestLimits::new(128).expect("test limits must be valid");
        assert_eq!(limits.max_body_bytes(), 128);
        assert_eq!(
            RequestLimits::new(0)
                .expect_err("zero must be refused")
                .kind,
            ConfigErrorKind::InvalidLimits
        );
    }

    #[test]
    fn debug_output_never_contains_credentials() {
        let token = BearerToken::new("top-secret").expect("test token must be valid");
        assert!(!format!("{token:?}").contains("top-secret"));
    }
}
