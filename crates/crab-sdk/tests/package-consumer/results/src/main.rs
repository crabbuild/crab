use crab_sdk::operation::{Options, Request};
use crab_sdk::remote::Remote;
use crab_sdk::results::{ResultsRead, ResultsResponse, RunQuery};

fn results_request<'a>(
    remote: Remote<'a>,
    query: RunQuery,
) -> Request<'a, ResultsResponse, Options> {
    remote.results().read(ResultsRead::List(query))
}

fn attachment_request<'a>(remote: Remote<'a>, hash: String) -> Request<'a, Vec<u8>, Options> {
    remote.results().blob(hash)
}

fn main() {
    let _ = (results_request, attachment_request);
}
