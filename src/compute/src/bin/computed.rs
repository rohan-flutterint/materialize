// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::path::PathBuf;
use std::process;

use anyhow::bail;
use axum::routing;
use mz_compute::server::CommunicationConfig;
use once_cell::sync::Lazy;
use tokio::select;
use tracing::info;

use mz_build_info::{build_info, BuildInfo};
use mz_dataflow_types::client::{ComputeClient, ComputeCommand, ComputeResponse, GenericClient};
use mz_dataflow_types::reconciliation::command::ComputeCommandReconcile;
use mz_dataflow_types::ConnectorContext;
use mz_orchestrator_tracing::TracingCliArgs;
use mz_ore::cli::{self, CliConfig};
use mz_ore::metrics::MetricsRegistry;
use mz_ore::now::SYSTEM_TIME;

use mz_pid_file::PidFile;
// Disable jemalloc on macOS, as it is not well supported [0][1][2].
// The issues present as runaway latency on load test workloads that are
// comfortably handled by the macOS system allocator. Consider re-evaluating if
// jemalloc's macOS support improves.
//
// [0]: https://github.com/jemalloc/jemalloc/issues/26
// [1]: https://github.com/jemalloc/jemalloc/issues/843
// [2]: https://github.com/jemalloc/jemalloc/issues/1467
#[cfg(all(not(target_os = "macos"), feature = "jemalloc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const BUILD_INFO: BuildInfo = build_info!();

pub static VERSION: Lazy<String> = Lazy::new(|| BUILD_INFO.human_version());

/// Independent dataflow server for Materialize.
#[derive(clap::Parser)]
#[clap(version = VERSION.as_str())]
struct Args {
    /// The address on which to listen for a connection from the controller.
    #[clap(
        long,
        env = "LISTEN_ADDR",
        value_name = "HOST:PORT",
        default_value = "127.0.0.1:2100"
    )]
    listen_addr: String,
    /// Number of dataflow worker threads.
    #[clap(short, long, env = "WORKERS", value_name = "W", default_value = "1")]
    workers: usize,
    /// Number of this computed process.
    #[clap(short = 'p', long, env = "PROCESS", value_name = "P")]
    process: Option<usize>,
    /// The addresses of all computed processes in the cluster.
    #[clap()]
    addresses: Vec<String>,

    /// An external ID to be supplied to all AWS AssumeRole operations.
    ///
    /// Details: <https://docs.aws.amazon.com/IAM/latest/UserGuide/id_roles_create_for-user_externalid.html>
    #[clap(long, value_name = "ID")]
    aws_external_id: Option<String>,
    /// Whether or not process should die when connection with ADAPTER is lost.
    #[clap(long)]
    linger: bool,
    /// Enable command reconciliation.
    #[clap(long, requires = "linger")]
    reconcile: bool,
    /// The address of the HTTP profiling UI.
    #[clap(long, value_name = "HOST:PORT")]
    http_console_addr: Option<String>,

    /// Where to write a pid lock file. Should only be used for local process orchestrators.
    #[clap(long, value_name = "PATH")]
    pid_file_location: Option<PathBuf>,

    #[clap(flatten)]
    tracing: TracingCliArgs,
}

#[tokio::main]
async fn main() {
    let args = cli::parse_args(CliConfig {
        env_prefix: Some("COMPUTED_"),
        enable_version_flag: true,
    });
    if let Err(err) = run(args).await {
        eprintln!("computed: fatal: {:#}", err);
        process::exit(1);
    }
}

fn create_communication_config(args: &Args) -> Result<CommunicationConfig, anyhow::Error> {
    let process = match args.process {
        None => 0,
        Some(process) if process >= args.addresses.len() => {
            bail!(
                "process index {process} out of range [0, {})",
                args.addresses.len()
            );
        }
        Some(process) => process,
    };
    Ok(CommunicationConfig {
        threads: args.workers,
        process,
        addresses: args.addresses.clone(),
    })
}

async fn run(args: Args) -> Result<(), anyhow::Error> {
    mz_ore::panic::set_abort_on_panic();
    mz_ore::tracing::configure("computed", &args.tracing).await?;

    let mut _pid_file = None;
    if let Some(pid_file_location) = &args.pid_file_location {
        _pid_file = Some(PidFile::open(&pid_file_location).unwrap());
    }

    if args.workers == 0 {
        bail!("--workers must be greater than 0");
    }
    let comm_config = create_communication_config(&args)?;

    info!("about to bind to {:?}", args.listen_addr);

    if let Some(addr) = args.http_console_addr {
        tracing::info!("serving computed HTTP server on {}", addr);
        mz_ore::task::spawn(
            || "computed_http_server",
            axum::Server::bind(&addr.parse()?).serve(
                mz_prof::http::router(&BUILD_INFO)
                    .route(
                        "/api/livez",
                        routing::get(mz_http_util::handle_liveness_check),
                    )
                    .into_make_service(),
            ),
        );
    }

    let config = mz_compute::server::Config {
        workers: args.workers,
        comm_config,
        metrics_registry: MetricsRegistry::new(),
        now: SYSTEM_TIME.clone(),
        connector_context: ConnectorContext::from_cli_args(
            &args.tracing.log_filter.inner,
            args.aws_external_id,
        ),
    };

    let serve_config = ServeConfig {
        listen_addr: args.listen_addr,
        linger: args.linger,
    };

    let (_server, client) = mz_compute::server::serve(config)?;
    let mut client: Box<dyn ComputeClient> = Box::new(client);
    if args.reconcile {
        client = Box::new(ComputeCommandReconcile::new(client))
    }

    serve(serve_config, client).await
}

struct ServeConfig {
    listen_addr: String,
    linger: bool,
}

async fn serve<G>(config: ServeConfig, mut client: G) -> Result<(), anyhow::Error>
where
    G: GenericClient<ComputeCommand, ComputeResponse>,
{
    let mut grpc_serve = mz_dataflow_types::client::tcp::grpc_computed_server(config.listen_addr);

    loop {
        // This select implies that the .recv functions of the clients must be cancellation safe.
        loop {
            select! {
                res = grpc_serve.recv() => {
                    match res {
                        Ok(cmd) => client.send(cmd).await.unwrap(),
                        Err(err) => {
                            tracing::warn!("Lost connection: {}", err);
                            break;
                        }
                    }
                },
                res = client.recv() => {
                    match res.unwrap() {
                        None => { },
                        Some(response) => {
                            match grpc_serve.send(response).await {
                                Ok(_) =>  { } ,
                                Err(err) => {
                                    tracing::warn!("Lost connection: {}", err);
                                    break;
                                }
                            }
                        }
                    }
                },
            }
        }
        if !config.linger {
            tracing::info!("coordinator connection gone; terminating");
            break;
        }
        tracing::info!("coordinator connection gone; waiting for reconnect");
    }

    Ok(())
}
