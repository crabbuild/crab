use std::{io, pin::Pin};

use axum::body::Body;
use bytes::Bytes;
use crab_cell_runtime::client::CellStateStream;
use crab_cell_runtime::client::Observed;
use crab_cell_runtime::registry::Query;
use futures_util::{Stream, StreamExt as _};

/// Adapts a bounded Cell state stream to one-at-a-time HTTP body chunks.
///
/// The encoder runs only after [`CellStateStream::emit`] has proven the
/// chunk's receipt. The adapter never prefetches inputs, and dropping the body
/// cancels the Cell stream so a disconnected client cannot retain admission.
pub fn state_observing_body<Q, Inputs, Encode>(
    stream: CellStateStream<Q>,
    inputs: Inputs,
    encode: Encode,
) -> Body
where
    Q: Query,
    Inputs: Stream<Item = Q::Input> + Send + 'static,
    Encode: FnMut(Observed<Q::Output>) -> io::Result<Bytes> + Send + 'static,
{
    let cancellation = stream.cancellation();
    let state = Adapter {
        stream,
        inputs: Box::pin(inputs),
        encode,
        cancellation,
        done: false,
    };
    let body = futures_util::stream::unfold(state, |mut state| async move {
        if state.done {
            return None;
        }
        let Some(input) = state.inputs.as_mut().next().await else {
            state.stream.finish();
            return None;
        };
        let item = match state.stream.emit(input).await {
            Ok(observed) => (state.encode)(observed),
            Err(error) => Err(io::Error::other(error.to_string())),
        };
        if item.is_err() {
            state.stream.finish();
            state.done = true;
        }
        Some((item, state))
    });
    Body::from_stream(body)
}

struct Adapter<Q, Inputs, Encode>
where
    Q: Query,
    Inputs: Stream<Item = Q::Input> + Send + 'static,
    Encode: FnMut(Observed<Q::Output>) -> io::Result<Bytes> + Send + 'static,
{
    stream: CellStateStream<Q>,
    inputs: Pin<Box<Inputs>>,
    encode: Encode,
    cancellation: crab_cell_runtime::client::StateStreamCancellation,
    done: bool,
}

impl<Q, Inputs, Encode> Drop for Adapter<Q, Inputs, Encode>
where
    Q: Query,
    Inputs: Stream<Item = Q::Input> + Send + 'static,
    Encode: FnMut(Observed<Q::Output>) -> io::Result<Bytes> + Send + 'static,
{
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
