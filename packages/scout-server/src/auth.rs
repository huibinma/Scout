//! Bearer token 鉴权 middleware —— 用 `subtle::ConstantTimeEq` 防 timing attack。
//!
//! BETA-32 T4：spec §6.2 安全硬规则要求 token 比较常量时间且不打印明文。
//! BETA-36 升级为**多 token + per-collection 权限**：每条 token 绑定一个
//! [`AuthedPrincipal`]（subject + collection 授权 + admin 标志）；middleware 命中后
//! 把 `Arc<AuthedPrincipal>` 塞进 request extensions——admin handlers 直接
//! `Extension<T>` 提取，MCP tools 经 rmcp 注入的 `http::request::Parts` 间接拿到
//! （spec §4「HTTP→MCP 穿透」）。

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use secrecy::{ExposeSecret, SecretString};
use subtle::ConstantTimeEq;

use crate::collections::{CollectionGrant, DaemonConfigFile};

/// 鉴权命中后的主体：token → 谁（subject）+ 能访问哪些 collection + 是否 admin。
///
/// 塞进 request extensions（`Arc` 包裹，clone 零拷贝）；audit 留痕的 subject 来源
/// （验收 ③）。
#[derive(Debug, Clone)]
pub struct AuthedPrincipal {
    /// audit 留痕主体（配置 `[[tokens]].subject`）。
    pub subject: String,
    /// collection 授权范围。
    pub grant: CollectionGrant,
    /// 是否可调 `/admin/*`。
    pub admin: bool,
}

impl AuthedPrincipal {
    /// 是否授权访问指定 collection。
    #[must_use]
    pub fn can_access(&self, collection_id: &str) -> bool {
        self.grant.allows(collection_id)
    }
}

/// 一条可匹配的 token 项。
#[derive(Clone)]
pub struct AuthTokenEntry {
    /// token 明文（`SecretString`，Debug 输出 REDACTED）。
    pub token: SecretString,
    /// 命中后注入的主体。
    pub principal: Arc<AuthedPrincipal>,
}

impl std::fmt::Debug for AuthTokenEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthTokenEntry")
            .field("token", &"<redacted>")
            .field("principal", &self.principal)
            .finish()
    }
}

/// 鉴权用的窄上下文 —— 只含 token 表，不依赖索引 / 模型。
///
/// 让 axum `State<Arc<AuthCtx>>` 在 unit test 中可以独立装配，
/// 不必为了测 middleware 真起一份 [`ServerCtx`](crate::config::ServerCtx)。
#[derive(Clone, Debug)]
pub struct AuthCtx {
    /// 全部可匹配 token。
    pub tokens: Vec<AuthTokenEntry>,
}

impl AuthCtx {
    /// 从（已校验的）配置文件构造。
    #[must_use]
    pub fn from_config_file(cfg: &DaemonConfigFile) -> Self {
        let tokens = cfg
            .tokens
            .iter()
            .map(|t| AuthTokenEntry {
                token: SecretString::from(t.token.expose_secret().to_owned()),
                principal: Arc::new(AuthedPrincipal {
                    subject: t.subject.clone(),
                    grant: CollectionGrant::from_patterns(&t.collections),
                    admin: t.admin,
                }),
            })
            .collect();
        Self { tokens }
    }

    /// 单 token 全权装配（legacy / 单测便捷入口）。
    #[must_use]
    pub fn single_full_access(token: SecretString, subject: &str) -> Self {
        Self {
            tokens: vec![AuthTokenEntry {
                token,
                principal: Arc::new(AuthedPrincipal {
                    subject: subject.to_string(),
                    grant: CollectionGrant::All,
                    admin: true,
                }),
            }],
        }
    }

    /// 常量时间匹配：**遍历全部条目不提前返回**（长度不同的条目跳过 `ct_eq`——
    /// subtle 要求两侧等长；单条内部仍是常量时间，条目数极少、遍历顺序不泄漏
    /// 匹配位置之外的信息）。
    #[must_use]
    pub fn match_token(&self, provided: &str) -> Option<Arc<AuthedPrincipal>> {
        let provided_bytes = provided.as_bytes();
        let mut matched: Option<Arc<AuthedPrincipal>> = None;
        for entry in &self.tokens {
            let expected = entry.token.expose_secret().as_bytes();
            if expected.len() == provided_bytes.len() && bool::from(provided_bytes.ct_eq(expected))
            {
                matched = Some(entry.principal.clone());
            }
        }
        matched
    }
}

/// 校验 `Authorization: Bearer <token>` header，逐条 subtle 常量时间比较；
/// 命中后把 `Arc<AuthedPrincipal>` 注入 request extensions。
///
/// 401 = 缺失 / 非 Bearer / 全部不匹配。
///
/// # Errors
///
/// 返回 [`StatusCode::UNAUTHORIZED`] 当：
/// - 缺 `Authorization` header
/// - header 不是 `Bearer ` 前缀
/// - token 与所有已配置条目均不匹配
pub async fn require_bearer(
    State(auth): State<Arc<AuthCtx>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let provided = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_string(); // 拷贝出来，let req 通过

    let principal = auth
        .match_token(&provided)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    req.extensions_mut().insert(principal);
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use axum::{
        body::Body,
        http::{header::AUTHORIZATION, Request, StatusCode},
        middleware::from_fn_with_state,
        routing::get,
        Extension, Router,
    };
    use tower::ServiceExt;

    use crate::collections::parse_config_toml;

    const TEST_TOKEN: &str = "test-token-32-chars-minimum-length";

    fn test_auth_ctx(token: &str) -> Arc<AuthCtx> {
        Arc::new(AuthCtx::single_full_access(
            SecretString::from(token.to_string()),
            "default",
        ))
    }

    fn test_app_with(auth: Arc<AuthCtx>) -> Router {
        // handler 回显 principal.subject，验证 extension 注入链路。
        Router::new()
            .route(
                "/p",
                get(|Extension(p): Extension<Arc<AuthedPrincipal>>| async move {
                    p.subject.clone()
                }),
            )
            .route_layer(from_fn_with_state(auth, require_bearer))
    }

    fn test_app(token: &str) -> Router {
        test_app_with(test_auth_ctx(token))
    }

    #[tokio::test]
    async fn missing_header_returns_401() {
        let app = test_app(TEST_TOKEN);
        let res = app
            .oneshot(Request::builder().uri("/p").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let app = test_app(TEST_TOKEN);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/p")
                    .header(AUTHORIZATION, "Bearer wrong-token-32-chars-minimum-length")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn correct_token_passes_and_injects_principal() {
        let app = test_app(TEST_TOKEN);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/p")
                    .header(AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"default", "handler 应能取到注入的 principal");
    }

    #[tokio::test]
    async fn wrong_length_returns_401() {
        let app = test_app(TEST_TOKEN);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/p")
                    .header(AUTHORIZATION, "Bearer short")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    // ===== BETA-36：多 token + principal =====

    const TOKEN_ZHANG: &str = "zhang-token-32-chars-minimum-len";
    const TOKEN_OPS: &str = "ops-token-32-chars-minimum-lengt";

    fn multi_token_ctx() -> Arc<AuthCtx> {
        let toml = format!(
            r#"
[[collections]]
id = "case-a"
roots = ["/a"]
[[collections]]
id = "case-b"
roots = ["/b"]
[[tokens]]
token = "{TOKEN_ZHANG}"
subject = "zhang.san"
collections = ["case-a"]
[[tokens]]
token = "{TOKEN_OPS}"
subject = "ops"
collections = ["*"]
admin = true
"#
        );
        let cfg = parse_config_toml(&toml).unwrap();
        Arc::new(AuthCtx::from_config_file(&cfg))
    }

    #[tokio::test]
    async fn multi_token_each_matches_own_principal() {
        let auth = multi_token_ctx();
        let p1 = auth.match_token(TOKEN_ZHANG).unwrap();
        assert_eq!(p1.subject, "zhang.san");
        assert!(p1.can_access("case-a"));
        assert!(!p1.can_access("case-b"), "zhang 无 case-b 权限");
        assert!(!p1.admin);

        let p2 = auth.match_token(TOKEN_OPS).unwrap();
        assert_eq!(p2.subject, "ops");
        assert!(p2.can_access("case-a"));
        assert!(p2.can_access("case-b"));
        assert!(p2.admin);
    }

    #[tokio::test]
    async fn multi_token_unknown_returns_none() {
        let auth = multi_token_ctx();
        assert!(auth
            .match_token("unknown-token-32-chars-minimum-l")
            .is_none());
    }

    #[tokio::test]
    async fn multi_token_middleware_injects_matching_subject() {
        let app = test_app_with(multi_token_ctx());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/p")
                    .header(AUTHORIZATION, format!("Bearer {TOKEN_ZHANG}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"zhang.san");
    }

    #[test]
    fn auth_token_entry_debug_redacted() {
        let auth = multi_token_ctx();
        let dbg = format!("{auth:?}");
        assert!(
            !dbg.contains(TOKEN_ZHANG) && !dbg.contains(TOKEN_OPS),
            "AuthCtx Debug 不得泄漏 token 明文：{dbg}"
        );
    }
}
