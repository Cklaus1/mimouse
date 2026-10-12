use env_logger::Env;
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
use mimouse::{
    capture_test,
    cli::{self, CliError},
    config::{self, Command, Config, ConfigError},
    emulation_test,
    ipc::{IpcError, IpcListenerCreationError},
    service::{Service, ServiceError},
};
use std::{future::Future, io, process};
use thiserror::Error;
use tokio::task::LocalSet;

#[derive(Debug, Error)]
enum MiMouseError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    IpcError(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[error(transparent)]
    Cli(#[from] CliError),
}

fn main() {
    // init logging
    let env = Env::default().filter_or("MIMOUSE_LOG_LEVEL", "info");
    env_logger::init_from_env(env);

    if let Err(e) = run() {
        log::error!("{e}");
        process::exit(1);
    }
}

fn run() -> Result<(), MiMouseError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(Command::TestEmulation(args)) => run_async(emulation_test::run(config, args)),
        Some(Command::TestCapture(args)) => run_async(capture_test::run(config, args)),
        Some(Command::Cli(cli_args)) => run_async(cli::run(cli_args, config)),
        // `mimouse` and `mimouse daemon` both run the service
        Some(Command::Daemon) | None => match run_async(run_service(config)) {
            Err(MiMouseError::Service(ServiceError::IpcListen(
                IpcListenerCreationError::AlreadyRunning,
            ))) => {
                log::info!("service already running!");
                Ok(())
            }
            r => r,
        },
    }
}

fn run_async<F, E>(f: F) -> Result<(), MiMouseError>
where
    F: Future<Output = Result<(), E>>,
    MiMouseError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

async fn run_service(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!(
        "Press {:?}{} to release the mouse",
        release_bind.keys,
        if release_bind.double_tap_ctrl {
            " or double-tap Ctrl"
        } else {
            ""
        }
    );
    service.run().await?;
    log::info!("service exited!");
    Ok(())
}
