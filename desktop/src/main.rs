#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use switchyard_app::{AppOptions, default_paths, start_host};
use tauri::{RunEvent, WebviewUrl, WebviewWindowBuilder};

#[derive(Parser)]
#[command(
    name = "switchya-desktop",
    version,
    about = "Switchya local agent workspace"
)]
struct Options {
    /// Gateway configuration. A private starter is created only when absent.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Persistent project and session data.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Enabled gateway client key used by the built-in agent.
    #[arg(long)]
    client_key_name: Option<String>,
    /// Write a private connection file for CLI handoff; never overwrites a file.
    #[arg(long)]
    write_launch_info: Option<PathBuf>,
}

fn run() -> Result<i32> {
    let args = Options::parse();
    let (config, data_dir) = match (args.config, args.data_dir) {
        (Some(config), Some(data)) => (config, data),
        (config, data) => {
            let (default_config, default_data) = default_paths()?;
            (
                config.unwrap_or(default_config),
                data.unwrap_or(default_data),
            )
        }
    };
    let data_dir =
        std::path::absolute(data_dir).context("could not resolve the workspace data directory")?;
    let mut options = AppOptions::new(config, data_dir.clone());
    options.client_key_name = args.client_key_name;

    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("could not start the workspace runtime")?,
    );
    let host = runtime.block_on(start_host(options))?;
    let launch_url = host.launch_url().parse::<tauri::Url>()?;
    let origin = launch_url.origin();
    let webview_data = data_dir.join("webview");
    let failure = Arc::new(Mutex::new(None));
    let setup_failure = failure.clone();

    let application = tauri::Builder::default()
        .setup(move |app| {
            let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(launch_url))
                .title("Switchya")
                .inner_size(1360.0, 900.0)
                .min_inner_size(720.0, 540.0)
                .data_directory(webview_data)
                // The local HTTP host owns authentication. No Tauri IPC
                // commands or remote-origin capabilities are granted.
                .on_navigation(move |url| url.origin() == origin)
                .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
                .build();
            if let Err(error) = window {
                // Tauri panics if a setup hook returns Err. Request a normal
                // exit instead so the host can stop and the GUI can report it.
                record_failure(
                    &setup_failure,
                    format!("Could not initialize the desktop webview: {error}"),
                );
                app.handle().exit(1);
            }
            Ok(())
        })
        .build(tauri::generate_context!());

    let application = match application {
        Ok(app) => app,
        Err(error) => {
            let _ = runtime.block_on(host.shutdown());
            return Err(error.into());
        }
    };
    let running_host = Arc::new(Mutex::new(Some(host)));
    let event_host = running_host.clone();
    let event_runtime = runtime.clone();
    let event_failure = failure.clone();
    let mut launch_info = args.write_launch_info;
    let exit_code = application.run_return(move |app, event| match event {
        RunEvent::Ready => {
            let failed = event_failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
            if !failed && let Some(path) = launch_info.take() {
                let result = event_host
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .map(|host| host.write_launch_info(path));
                if let Some(Err(error)) = result {
                    record_failure(
                        &event_failure,
                        format!("Could not write CLI connection information: {error}"),
                    );
                    app.exit(1);
                }
            }
        }
        RunEvent::Exit => {
            stop_host(&event_host, &event_runtime, &event_failure);
        }
        _ => {}
    });
    // Normally Exit already consumed the host. This also covers an event loop
    // returning without that event, without shutting down the host twice.
    stop_host(&running_host, &runtime, &failure);
    let failure = failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(message) = failure {
        return Err(anyhow::anyhow!(message));
    }
    Ok(exit_code)
}

fn record_failure(failure: &Mutex<Option<String>>, message: String) {
    let mut failure = failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if failure.is_none() {
        *failure = Some(message);
    }
}

fn stop_host(
    host: &Mutex<Option<switchyard_app::RunningHost>>,
    runtime: &tokio::runtime::Runtime,
    failure: &Mutex<Option<String>>,
) {
    let host = host
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(host) = host
        && let Err(error) = runtime.block_on(host.shutdown())
    {
        record_failure(
            failure,
            format!("Could not finish workspace shutdown: {error}"),
        );
    }
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            report_error(&format!(
                "Switchya could not complete startup or shutdown.\n\n{error:#}"
            ));
            std::process::exit(1);
        }
    }
}

fn report_error(message: &str) {
    eprintln!("{message}");
    #[cfg(windows)]
    {
        let title: Vec<u16> = "Switchya\0".encode_utf16().collect();
        let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
        // Both zero-terminated buffers stay alive for the modal call.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                windows_sys::Win32::UI::WindowsAndMessaging::MB_OK
                    | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        }
    }
}
