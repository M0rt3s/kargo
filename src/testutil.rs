//! Helpers shared by unit tests.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use axum::Router;
use tokio::net::TcpListener;

/// Serve `router` on an ephemeral local port and return its base URL.
pub(crate) async fn serve(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

/// A fresh hit counter.
pub(crate) fn counter() -> Arc<AtomicUsize> {
    Arc::default()
}
