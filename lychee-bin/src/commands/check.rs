use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use indicatif::ProgressBar;
use indicatif::ProgressStyle;
use lychee_lib::Result;
use lychee_lib::Status;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::formatters::response::ResponseFormatter;
use crate::verbosity::{Verbosity, WarnLevel};
use crate::{cache::Cache, stats::ResponseStats, ExitCode};
use lychee_lib::{Client, Request, Response};

use super::CommandParams;

pub(crate) async fn check<S>(
    params: CommandParams<S>,
) -> Result<(ResponseStats, Arc<Cache>, ExitCode)>
where
    S: futures::Stream<Item = Result<Request>>,
{
    // Setup
    let (send_req, recv_req) = mpsc::channel(params.cfg.max_concurrency);
    let (send_resp, recv_resp) = mpsc::channel(params.cfg.max_concurrency);
    let max_concurrency = params.cfg.max_concurrency;
    let stats = if params.cfg.verbose.is_verbose() {
        ResponseStats::extended()
    } else {
        ResponseStats::default()
    };
    let cache_ref = params.cache.clone();

    let client = params.client;
    let cache = params.cache;
    let accept = params.cfg.accept;

    let pb = if params.cfg.no_progress {
        None
    } else {
        Some(init_progress_bar())
    };

    // Start receiving requests
    tokio::spawn(request_channel_task(
        recv_req,
        send_resp,
        max_concurrency,
        client,
        cache,
        accept,
    ));

    let show_results_task = tokio::spawn(progress_bar_task(
        recv_resp,
        params.cfg.verbose,
        pb.clone(),
        Arc::new(params.formatter),
        stats,
    ));

    // Wait until all messages are sent
    send_inputs_loop(params.requests, send_req, pb).await?;

    // Wait until all responses are received
    let result = show_results_task.await?;
    let (pb, stats) = result?;

    // Note that print statements may interfere with the progress bar, so this
    // must go before printing the stats
    if let Some(pb) = &pb {
        pb.finish_and_clear();
    }

    let code = if stats.is_success() {
        ExitCode::Success
    } else {
        ExitCode::LinkCheckFailure
    };
    Ok((stats, cache_ref, code))
}

// drops the `send_req` channel on exit
// required for the receiver task to end, which closes send_resp, which allows
// the show_results_task to finish
async fn send_inputs_loop<S>(
    requests: S,
    send_req: mpsc::Sender<Result<Request>>,
    bar: Option<ProgressBar>,
) -> Result<()>
where
    S: futures::Stream<Item = Result<Request>>,
{
    tokio::pin!(requests);
    while let Some(request) = requests.next().await {
        let request = request?;
        if let Some(pb) = &bar {
            pb.inc_length(1);
            pb.set_message(request.to_string());
        };
        send_req
            .send(Ok(request))
            .await
            .expect("Cannot send request");
    }
    Ok(())
}

/// Reads from the request channel and updates the progress bar status
async fn progress_bar_task(
    mut recv_resp: mpsc::Receiver<Response>,
    verbose: Verbosity<WarnLevel>,
    pb: Option<ProgressBar>,
    formatter: Arc<Box<dyn ResponseFormatter>>,
    mut stats: ResponseStats,
) -> Result<(Option<ProgressBar>, ResponseStats)> {
    while let Some(response) = recv_resp.recv().await {
        show_progress(&mut io::stdout(), &pb, &response, &formatter, &verbose)?;
        stats.add(response);
    }
    Ok((pb, stats))
}

fn init_progress_bar() -> ProgressBar {
    let bar = ProgressBar::new_spinner().with_style(
        ProgressStyle::with_template(
            "{spinner:.197.bright} {pos}/{len:.dim} ETA {eta} {bar:.dim} {wide_msg}",
        )
        .expect("Valid progress bar"),
    );
    bar.set_length(0);
    bar.set_message("Extracting links");
    // report status _at least_ every 500ms
    bar.enable_steady_tick(Duration::from_millis(500));
    bar
}

async fn request_channel_task(
    recv_req: mpsc::Receiver<Result<Request>>,
    send_resp: mpsc::Sender<Response>,
    max_concurrency: usize,
    client: Client,
    cache: Arc<Cache>,
    accept: Option<HashSet<u16>>,
) {
    futures::StreamExt::for_each_concurrent(
        ReceiverStream::new(recv_req),
        max_concurrency,
        |request: Result<Request>| async {
            let request = request.expect("cannot read request");
            let response = handle(&client, cache.clone(), request, accept.clone()).await;

            send_resp
                .send(response)
                .await
                .expect("cannot send response to queue");
        },
    )
    .await;
}

/// Handle a single request
async fn handle(
    client: &Client,
    cache: Arc<Cache>,
    request: Request,
    accept: Option<HashSet<u16>>,
) -> Response {
    let uri = request.uri.clone();
    if let Some(v) = cache.get(&uri) {
        // Found a cached request
        // Overwrite cache status in case the URI is excluded in the
        // current run
        let status = if client.is_excluded(&uri) {
            Status::Excluded
        } else {
            // Can't impl `Status::from(v.value().status)` here because the
            // `accepted` status codes might have changed from the previous run
            // and they may have an impact on the interpretation of the status
            // code.
            Status::from_cache_status(v.value().status, accept)
        };
        return Response::new(uri.clone(), status, request.source);
    }

    // Request was not cached; run a normal check
    //
    // This can panic when the Url could not be parsed to a Uri.
    // See https://github.com/servo/rust-url/issues/554
    // See https://github.com/seanmonstar/reqwest/issues/668
    // TODO: Handle error as soon as https://github.com/seanmonstar/reqwest/pull/1399 got merged
    let response = client.check(request).await.expect("cannot check URI");

    // - Never cache filesystem access as it is fast already so caching has no
    //   benefit.
    // - Skip caching unsupported URLs as they might be supported in a
    //   future run.
    // - Skip caching excluded links; they might not be excluded in the next run
    let status = response.status();
    if !uri.is_file() && !status.is_excluded() && !status.is_unsupported() {
        cache.insert(uri, status.into());
    }
    response
}

fn show_progress(
    output: &mut dyn Write,
    progress_bar: &Option<ProgressBar>,
    response: &Response,
    formatter: &Arc<Box<dyn ResponseFormatter>>,
    verbose: &Verbosity<WarnLevel>,
) -> Result<()> {
    let out = formatter.write_response(response)?;
    if let Some(pb) = progress_bar {
        pb.inc(1);
        pb.set_message(out.clone());
        if verbose.is_verbose() {
            pb.println(out);
        }
    } else if verbose.is_verbose()
        || (!response.status().is_success() && !response.status().is_excluded())
    {
        writeln!(output, "{out}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use log::info;
    use lychee_lib::{CacheStatus, InputSource, ResponseBody, Uri};

    use crate::formatters;

    use super::*;

    #[test]
    fn test_skip_cached_responses_in_progress_output() {
        let mut buf = Vec::new();
        let response = Response(
            InputSource::Stdin,
            ResponseBody {
                uri: Uri::try_from("http://127.0.0.1").unwrap(),
                status: Status::Cached(CacheStatus::Ok(200)),
            },
        );
        let formatter: Arc<Box<dyn ResponseFormatter>> =
            Arc::new(Box::new(formatters::response::Raw::new()));
        show_progress(
            &mut buf,
            &None,
            &response,
            &formatter,
            &Verbosity::new(0, 0),
        )
        .unwrap();

        info!("{:?}", String::from_utf8_lossy(&buf));
        assert!(buf.is_empty());
    }
}
