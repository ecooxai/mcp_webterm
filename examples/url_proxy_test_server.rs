//! Isolated URL-proxy harness: PROXY_TEST_LISTEN=127.0.0.1:11080 cargo run --example url_proxy_test_server
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let address = std::env::var("PROXY_TEST_LISTEN").unwrap_or_else(|_| "127.0.0.1:11080".into());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    let router = webterm::generic_proxy::router()?;
    println!("Test URL proxy listening on {address}");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
