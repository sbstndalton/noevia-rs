//! Keep completion accounting progressing even under a ready accept backlog.
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinError, JoinSet};

pub(super) enum Event {
    Stop,
    Joined(Option<Result<(), JoinError>>),
    Accepted(std::io::Result<(TcpStream, std::net::SocketAddr)>),
}

pub(super) async fn next_event(
    listener: &TcpListener,
    tasks: &mut JoinSet<()>,
    shutdown: &mut watch::Receiver<bool>,
) -> Event {
    if *shutdown.borrow() {
        return Event::Stop;
    }
    tokio::select! {
        biased;
        _ = shutdown.changed() => Event::Stop,
        joined = tasks.join_next(), if !tasks.is_empty() => Event::Joined(joined),
        accepted = listener.accept() => Event::Accepted(accepted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn completed_connections_join_while_accept_is_ready(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let _client = TcpStream::connect(listener.local_addr()?).await?;
        let mut tasks = JoinSet::new();
        let handles: Vec<_> = (0..32).map(|_| tasks.spawn(async {})).collect();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !handles.iter().all(|h| h.is_finished()) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let (_stop, mut rx) = watch::channel(false);
        for _ in 0..32 {
            let event = tokio::time::timeout(
                Duration::from_secs(1),
                next_event(&listener, &mut tasks, &mut rx),
            )
            .await?;
            assert!(
                matches!(event, Event::Joined(Some(Ok(())))),
                "ready accepts starved completed connection joins"
            );
        }
        assert!(tasks.is_empty());
        // The same queued connection is still available after every completion.
        assert!(matches!(
            next_event(&listener, &mut tasks, &mut rx).await,
            Event::Accepted(Ok(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_wins_over_ready_accept_and_completion(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let _client = TcpStream::connect(listener.local_addr()?).await?;
        let mut tasks = JoinSet::new();
        let handle = tasks.spawn(async {});
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
        let (_stop, mut rx) = watch::channel(true);
        assert!(matches!(
            next_event(&listener, &mut tasks, &mut rx).await,
            Event::Stop
        ));
        assert_eq!(tasks.len(), 1);
        tasks.join_next().await.transpose()?;
        Ok(())
    }
}
