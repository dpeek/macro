//! MCP server binary that serves the DCS AI toolset over HTTP.
//!
//! This binary spins up a Streamable HTTP MCP server exposing the same
//! tools that are available in the DCS chat/stream API. Public deployments
//! use OAuth 2.1 backed by FusionAuth; private deployments accept Macro bearer
//! tokens without starting the OAuth broker.

mod config;
mod context;
mod markdown_images;
mod tool_response;
mod tool_service;
use anyhow::Context;
use config::Config;
use context::build_context;
use macro_entrypoint::MacroEntrypoint;
use mcp_auth_proxy::domain::service::McpAuthProxyService;
use mcp_auth_proxy::inbound::axum_router::{internal_mcp_router, mcp_router};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::Arc;
use tokio::time::Duration;
use tokio_util::task::TaskTracker;
use tool_service::AuthenticatedToolService;

const AUTH_PROXY_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);
const EVENT_BROKER_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
#[tracing::instrument(err)]
async fn main() -> anyhow::Result<()> {
    MacroEntrypoint::default().init();

    let config = Config::from_env()?;

    // Base URL of the Macro web app, used to build links to Macro items in MCP
    // responses.
    let item_base_url = config.app_base_url.as_ref().to_string();
    let static_file_base_url =
        url::Url::parse(macro_service_urls::StaticFileServiceUrl::new()?.as_ref())?;

    let event_broker_tracker = TaskTracker::new();
    let context = build_context(&config, event_broker_tracker.clone()).await?;

    let auth_proxy = context.auth_proxy.clone();
    let jwt_args = context.jwt_args.clone();
    let mcp_public_host = context.mcp_public_host.clone();

    // Create the MCP service with authenticated tool handler
    let mcp_service = StreamableHttpService::new(
        move || {
            let tools = ai_tools::tools_for(ai_tools::AiHost::Mcp);
            Ok(AuthenticatedToolService::new(
                tools.toolset,
                context.tool_context.clone(),
                item_base_url.clone(),
                static_file_base_url.clone(),
            ))
        },
        Arc::new(LocalSessionManager::default()),
        {
            let mut config = StreamableHttpServerConfig::default().with_allowed_hosts([
                mcp_public_host,
                "localhost".into(),
                "127.0.0.1".into(),
                "gateway.macro.com".into(),
                "dev-gateway.macro.com".into(),
            ]);
            config.stateful_mode = false;
            config.json_response = true;
            config
        },
    );

    let app = if let Some(auth_proxy) = auth_proxy {
        // Spawn background cleanup for expired OAuth entries.
        let cleanup_state = auth_proxy.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(AUTH_PROXY_CLEANUP_INTERVAL);
            loop {
                interval.tick().await;
                if let Err(error) = cleanup_state.cleanup_expired().await {
                    tracing::error!(error=?error, "auth proxy cleanup task failed");
                }
            }
        });
        mcp_router(auth_proxy, jwt_args, mcp_service)
    } else {
        internal_mcp_router(jwt_args, mcp_service)
    };

    let port = config.port;
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .context("failed to bind MCP server")?;

    tracing::info!("MCP server listening on http://{addr}/mcp");

    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(macro_entrypoint::shutdown_signal())
        .await
        .context("MCP server error");

    tracing::info!("waiting for event broker publishes to drain");
    event_broker_tracker.close();
    match tokio::time::timeout(EVENT_BROKER_DRAIN_TIMEOUT, event_broker_tracker.wait()).await {
        Ok(()) => tracing::info!("event broker publishes drained"),
        Err(error) => {
            tracing::warn!(
                error=?error,
                timeout_seconds = EVENT_BROKER_DRAIN_TIMEOUT.as_secs(),
                "timed out waiting for event broker publishes to drain"
            );
        }
    }

    server_result
}
