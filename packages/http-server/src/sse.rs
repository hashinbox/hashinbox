//! Server-sent event streams answered a window at a time: [`windowed`] ends
//! each answer after a window with `event: close`, and the reader asks
//! again from the last event `id:` it read (a route's `?cursor=`). So no
//! answer outlives its window, and each reconnect is a new request, its
//! credentials checked again.

use std::{convert::Infallible, time::Duration};

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::{Stream, StreamExt as _, future::Either};

/// The default window: how long one answer streams.
pub const SSE_WINDOW: Duration = Duration::from_secs(15);

/// `events` as a `text/event-stream` answer for `window`, kept alive by
/// comments; then `event: close` (data `{}`, `retry: 0`) and its end. A
/// stream that ends first sends no `close`: it is done, and the reader
/// should not ask again.
pub fn windowed(
    events: impl Stream<Item = Event> + Send + 'static,
    window: Duration,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let state = (events.boxed(), Box::pin(tokio::time::sleep(window)), false);
    let events = futures_util::stream::unfold(state, |(mut events, mut end, closed)| async move {
        if closed {
            return None;
        }
        match futures_util::future::select(end.as_mut(), events.next()).await {
            Either::Left(((), _)) => {
                let close = Event::default().event("close").data("{}");
                Some((Ok(close.retry(Duration::ZERO)), (events, end, true)))
            }
            Either::Right((event, _)) => Some((Ok(event?), (events, end, false))),
        }
    });
    Sse::new(events).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, response::IntoResponse as _};
    use futures_util::StreamExt as _;

    use super::*;

    /// The whole body of `answer`, as text.
    async fn text(
        answer: Sse<impl Stream<Item = Result<Event, Infallible>> + Send + 'static>,
    ) -> String {
        let body = Body::new(answer.into_response().into_body());
        let mut frames = body.into_data_stream();
        let mut text = String::new();
        while let Some(frame) = frames.next().await {
            text.push_str(&String::from_utf8_lossy(&frame.expect("a frame")));
        }
        text
    }

    #[tokio::test]
    async fn a_stream_longer_than_its_window_ends_with_close() {
        let events = futures_util::stream::iter([Event::default().id("1").data("a")])
            .chain(futures_util::stream::pending());
        let text = text(windowed(events, Duration::from_millis(20))).await;
        assert_eq!(
            text,
            "id: 1\ndata: a\n\nevent: close\ndata: {}\nretry: 0\n\n"
        );
    }

    #[tokio::test]
    async fn a_stream_done_within_its_window_ends_without_close() {
        let events = futures_util::stream::iter([Event::default().data("a")]);
        let text = text(windowed(events, SSE_WINDOW)).await;
        assert_eq!(text, "data: a\n\n");
    }
}
