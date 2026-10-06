use tokio::sync::watch;

pub async fn wait(cancellation: Option<watch::Receiver<bool>>) {
    let Some(mut cancellation) = cancellation else {
        std::future::pending::<()>().await;
        return;
    };

    loop {
        if *cancellation.borrow() {
            return;
        }
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}
