#![cfg_attr(target_os = "windows", windows_subsystem = "console")]

use agentranch::{
    api::{
        bridge::{BridgeOwner, BridgeWake, ServiceBridge},
        http::{serve_connection, EventHubAccess, HttpApp},
        lifecycle::{
            console_shutdown_requested, install_console_shutdown, shutdown_with_deadline, DaemonArgs,
            DaemonRuntime, Ingress, ShutdownOutcome, ShutdownState,
        },
    },
    config::Mode,
    core::service::{ServiceHandle, ServiceOptions},
    worker_identity::{validate_worker, worker_executable},
};
use std::{net::TcpListener, sync::Arc, time::Duration};

/// Total shutdown budget from the design: over-budget exits nonzero.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

fn main() {
    if let Err(category) = run() {
        eprintln!("agentranchd startup failed: {category}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), &'static str> {
    let raw_args = std::env::args_os().skip(1)
        .map(|arg| arg.into_string().map_err(|_| "invalid_arguments"))
        .collect::<Result<Vec<_>, _>>()?;
    let args = DaemonArgs::parse(raw_args.iter().map(String::as_str)).map_err(|_| "invalid_arguments")?;
    let executable = std::env::current_exe().map_err(|_| "worker_identity_failed")?;
    let worker = worker_executable(&executable).map_err(|_| "worker_identity_failed")?;
    if args.mode == Mode::Local {
        validate_worker(&worker).map_err(|_| "worker_identity_failed")?;
    }

    let mut daemon = DaemonRuntime::start(args.clone()).map_err(|_| "startup_preparation_failed")?;
    let console = install_console_shutdown();
    #[cfg(windows)]
    if !console {
        // Serving without a working shutdown signal would leave a stale
        // discovery file behind; refuse to start instead.
        return Err("console_handler_failed");
    }
    #[cfg(not(windows))]
    let _ = console;

    let wake = BridgeWake::new();
    let service = ServiceHandle::spawn(
        ServiceOptions {
            mode: args.mode.clone(),
            state_dir: args.state_dir.clone(),
            worker_exe: worker,
            gateway_key: None,
            gateway_model_token: None,
        },
        wake.repaint(),
    )
    .map_err(|_| "core_start_failed")?;
    // The bridge snapshots the core synchronously, so the hub already holds
    // the initial snapshot before any listener work starts.
    let bridge = ServiceBridge::with_wake(Box::new(service), wake);
    let client = Arc::new(bridge.client());
    let events = Arc::new(EventHubAccess::new(bridge.hub()));
    let ingress = Arc::new(Ingress::new());
    let app = Arc::new(
        HttpApp::new(args.mode.clone(), client, events.clone())
            .with_state_dir(args.state_dir.clone())
            .with_ingress(ingress.clone()),
    );
    let owner = BridgeOwner::spawn(bridge);

    let listener = daemon.take_listener().map_err(|_| "listener_unavailable")?;
    listener.set_nonblocking(true).map_err(|_| "listener_unavailable")?;
    // Listener bound, bridge owner draining, initial snapshot published: only
    // now does the discovery file promise a usable endpoint.
    daemon.mark_ready();
    // Discovery identity must be the transport identity clients read from
    // `/v1/health` and SSE ids, not a second independent UUID.
    daemon
        .publish_endpoint_with(events.instance_id())
        .map_err(|_| "endpoint_publish_failed")?;
    let shutdown = ShutdownState::new(ingress, owner, events, daemon.take_endpoint());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| "runtime_unavailable")?;
    let outcome = match runtime.block_on(serve(listener, app, shutdown)) {
        Ok(outcome) => outcome,
        Err(category) => {
            // The shutdown state was dropped on the error path, so the owner
            // still stops itself and the discovery file is already removed.
            drop(runtime);
            return Err(category);
        }
    };

    if outcome.timed_out {
        // The owner may still sit in a blocking wait; never let runtime
        // teardown stretch the shutdown budget or fabricate success.
        runtime.shutdown_background();
        eprintln!("agentranchd shutdown exceeded its budget: receipt state unknown");
        std::process::exit(3);
    }
    drop(runtime);
    Ok(())
}

async fn serve(
    listener: TcpListener,
    app: Arc<HttpApp>,
    shutdown: ShutdownState,
) -> Result<ShutdownOutcome, &'static str> {
    let listener =
        tokio::net::TcpListener::from_std(listener).map_err(|_| "listener_unavailable")?;
    loop {
        if console_shutdown_requested() {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(50), listener.accept()).await {
            Ok(Ok((stream, _))) => {
                let app = app.clone();
                tokio::spawn(async move {
                    let _ = serve_connection(stream, app).await;
                });
            }
            // An unrecoverable accept error ends serving; the same shutdown
            // path still drains the core and removes discovery.
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    Ok(shutdown_with_deadline(shutdown, SHUTDOWN_BUDGET).await)
}
