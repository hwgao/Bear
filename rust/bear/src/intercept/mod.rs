// SPDX-License-Identifier: GPL-3.0-or-later

//! The module contains the intercept reporting and collecting functionality.
//!
//! When a command execution is intercepted, the interceptor sends the event to the collector.
//! This happens in two different processes, requiring a communication channel between these
//! processes.
//!
//! The module provides abstractions for the reporter and the collector. And it also defines
//! the data structures that are used to represent the events.

use crate::intercept::supervise::supervise;
use crate::{args, config};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::{env, fmt, thread};

pub mod persistence;
pub mod supervise;
pub mod tcp;

/// Declare the environment variables used by the intercept mode.
pub const KEY_DESTINATION: &str = "INTERCEPT_REPORTER_ADDRESS";
pub const KEY_PRELOAD_PATH: &str = "LD_PRELOAD";

/// Represents the remote sink of supervised process events.
///
/// This allows the reporters to send events to a remote collector.
pub trait Reporter {
    fn report(&self, event: Event) -> Result<(), anyhow::Error>;
}

/// Represents the local sink of supervised process events.
///
/// The collector is responsible for collecting the events from the reporters.
///
/// To share the collector between threads, we use the `Arc` type to wrap the
/// collector. This way we can clone the collector and send it to other threads.
pub trait Collector {
    /// Returns the address of the collector.
    ///
    /// The address is in the format of `ip:port`.
    fn address(&self) -> String;

    /// Collects the events from the reporters.
    ///
    /// The events are sent to the given destination channel.
    ///
    /// The function returns when the collector is stopped. The collector is stopped
    /// when the `stop` method invoked (from another thread).
    fn collect(&self, destination: Sender<Envelope>) -> Result<(), anyhow::Error>;

    /// Stops the collector.
    fn stop(&self) -> Result<(), anyhow::Error>;
}

/// Envelope is a wrapper around the event.
///
/// It contains the reporter id, the timestamp of the event and the event itself.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct Envelope {
    pub rid: ReporterId,
    pub timestamp: u64,
    pub event: Event,
}

impl fmt::Display for Envelope {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Envelope rid={}, timestamp={}, event={}",
            self.rid.0, self.timestamp, self.event
        )
    }
}

/// Represent a relevant life cycle event of a process.
///
/// In the current implementation, we only have one event, the `Started` event.
/// This event is sent when a process is started. It contains the process id
/// and the execution information.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct Event {
    pub pid: ProcessId,
    pub execution: Execution,
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Event pid={}, execution={}", self.pid.0, self.execution)
    }
}

/// Execution is a representation of a process execution.
///
/// It does not contain information about the outcome of the execution,
/// like the exit code or the duration of the execution. It only contains
/// the information that is necessary to reproduce the execution.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct Execution {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub working_dir: PathBuf,
    pub environment: HashMap<String, String>,
}

impl fmt::Display for Execution {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Execution path={}, args=[{}]",
            self.executable.display(),
            self.arguments.join(",")
        )
    }
}

/// Reporter id is a unique identifier for a reporter.
///
/// It is used to identify the process that sends the execution report.
/// Because the OS PID is not unique across a single build (PIDs are
/// recycled), we need to use a new unique identifier to identify the process.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct ReporterId(pub u64);

/// Process id is a OS identifier for a process.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct ProcessId(pub u32);

/// The service is responsible for collecting the events from the supervised processes.
///
/// The service is implemented as TCP server that listens on a random port on the loopback
/// interface. The address of the service can be obtained by the `address` method.
///
/// The service is started in a separate thread to dispatch the events to the consumer.
/// The consumer is a function that receives the events from the service and processes them.
/// It also runs in a separate thread. The reason for having two threads is to avoid blocking
/// the main thread of the application and decouple the collection from the processing.
pub(crate) struct CollectorService {
    collector: Arc<dyn Collector>,
    network_thread: Option<thread::JoinHandle<()>>,
    output_thread: Option<thread::JoinHandle<()>>,
}

impl CollectorService {
    /// Creates a new intercept service.
    ///
    /// The `consumer` is a function that receives the events and processes them.
    /// The function is executed in a separate thread.
    pub fn new<F>(consumer: F) -> anyhow::Result<Self>
    where
        F: FnOnce(Receiver<Envelope>) -> anyhow::Result<()>,
        F: Send + 'static,
    {
        let collector = tcp::CollectorOnTcp::new()?;
        let collector_arc = Arc::new(collector);
        let (sender, receiver) = channel();

        let collector_in_thread = collector_arc.clone();
        let collector_thread = thread::spawn(move || {
            let result = collector_in_thread.collect(sender);
            if let Err(e) = result {
                log::error!("Failed to collect events: {}", e);
            }
        });
        let output_thread = thread::spawn(move || {
            let result = consumer(receiver);
            if let Err(e) = result {
                log::error!("Failed to process events: {}", e);
            }
        });

        log::debug!("Collector service started at {}", collector_arc.address());
        Ok(CollectorService {
            collector: collector_arc,
            network_thread: Some(collector_thread),
            output_thread: Some(output_thread),
        })
    }

    /// Returns the address of the service.
    pub fn address(&self) -> String {
        self.collector.address()
    }
}

impl Drop for CollectorService {
    /// Shuts down the service.
    fn drop(&mut self) {
        // TODO: log the shutdown of the service and any errors
        self.collector.stop().expect("Failed to stop the collector");
        if let Some(thread) = self.network_thread.take() {
            thread.join().expect("Failed to join the collector thread");
        }
        if let Some(thread) = self.output_thread.take() {
            thread.join().expect("Failed to join the output thread");
        }
    }
}

/// The environment for the intercept mode.
///
/// Running the build command requires a specific environment. The environment we
/// need for intercepting the child processes is different for each intercept mode.
///
/// The `Wrapper` mode requires a temporary directory with the executables that will
/// be used to intercept the child processes. The executables are hard linked to the
/// temporary directory.
///
/// The `Preload` mode requires the path to the preload library that will be used to
/// intercept the child processes.
pub(crate) enum InterceptEnvironment {
    Wrapper {
        bin_dir: tempfile::TempDir,
        address: String,
        collector: CollectorService,
    },
    Preload {
        path: PathBuf,
        address: String,
        collector: CollectorService,
    },
}

impl InterceptEnvironment {
    /// Creates a new intercept environment.
    ///
    /// The `config` is the intercept configuration that specifies the mode and the
    /// required parameters for the mode. The `collector` is the service to collect
    /// the execution events.
    pub fn new(config: &config::Intercept, collector: CollectorService) -> anyhow::Result<Self> {
        let address = collector.address();
        let result = match config {
            config::Intercept::Wrapper {
                path,
                directory,
                executables,
            } => {
                // Create a temporary directory and populate it with the executables.
                let bin_dir = tempfile::TempDir::with_prefix_in(directory, "bear-")?;
                for executable in executables {
                    std::fs::hard_link(executable, path)?;
                }
                InterceptEnvironment::Wrapper {
                    bin_dir,
                    address,
                    collector,
                }
            }
            config::Intercept::Preload { path } => InterceptEnvironment::Preload {
                path: path.clone(),
                address,
                collector,
            },
        };
        Ok(result)
    }

    /// Executes the build command in the intercept environment.
    ///
    /// The method is blocking and waits for the build command to finish.
    /// The method returns the exit code of the build command. Result failure
    /// indicates that the build command failed to start.
    pub fn execute_build_command(&self, input: args::BuildCommand) -> anyhow::Result<ExitCode> {
        // TODO: record the execution of the build command

        let environment = self.environment();
        let process = input.arguments[0].clone();
        let arguments = input.arguments[1..].to_vec();

        let mut child = Command::new(process);

        let exit_status = supervise(child.args(arguments).envs(environment))?;
        log::info!("Execution finished with status: {:?}", exit_status);

        // The exit code is not always available. When the process is killed by a signal,
        // the exit code is not available. In this case, we return the `FAILURE` exit code.
        let exit_code = exit_status
            .code()
            .map(|code| ExitCode::from(code as u8))
            .unwrap_or(ExitCode::FAILURE);

        Ok(exit_code)
    }

    /// Returns the environment variables for the intercept environment.
    ///
    /// The environment variables are different for each intercept mode.
    /// It does not change the original environment variables, but creates
    /// the environment variables that are required for the intercept mode.
    fn environment(&self) -> Vec<(String, String)> {
        match self {
            InterceptEnvironment::Wrapper {
                bin_dir, address, ..
            } => {
                let path_original = env::var("PATH").unwrap_or_else(|_| String::new());
                let path_updated = InterceptEnvironment::insert_to_path(
                    &path_original,
                    Self::path_to_string(bin_dir.path()),
                );
                vec![
                    ("PATH".to_string(), path_updated),
                    (KEY_DESTINATION.to_string(), address.clone()),
                ]
            }
            InterceptEnvironment::Preload { path, address, .. } => {
                let path_original = env::var(KEY_PRELOAD_PATH).unwrap_or_else(|_| String::new());
                let path_updated = InterceptEnvironment::insert_to_path(
                    &path_original,
                    Self::path_to_string(path),
                );
                vec![
                    (KEY_PRELOAD_PATH.to_string(), path_updated),
                    (KEY_DESTINATION.to_string(), address.clone()),
                ]
            }
        }
    }

    /// Manipulate a `PATH` like environment value by inserting the `first` path into
    /// the original value. It removes the `first` path if it already exists in the
    /// original value. And it inserts the `first` path at the beginning of the value.
    fn insert_to_path(original: &str, first: String) -> String {
        let mut paths: Vec<_> = original.split(':').filter(|it| it != &first).collect();
        paths.insert(0, first.as_str());
        paths.join(":")
    }

    fn path_to_string(path: &Path) -> String {
        path.to_str().unwrap_or("").to_string()
    }
}
