/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2025-2026 Shomy
*/
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use colored::Colorize;
use env_logger::fmt::Formatter;
use log::{Level, LevelFilter, Record};

pub const LOG_FILE_NAME: &str = "antumbra.log";
pub const LOGGER_PREIX: &str = "Antumbra";
pub const INFO_SYMBOL: &str = "✦";
pub const WARN_SYMBOL: &str = "✧";
pub const ERROR_SYMBOL: &str = "❂";

fn log_path() -> PathBuf {
    // Use a stable per-user location instead of the process working directory.
    // GUI launches, shortcuts, and relaunches can all have different CWDs.
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Antumbra")
        .join("logs")
        .join(LOG_FILE_NAME)
}

fn open_log_file() -> (PathBuf, Option<File>) {
    let path = log_path();

    if let Some(parent) = path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            eprintln!("Antumbra: couldn't create log directory '{}': {error}", parent.display());
        } else {
            match File::options().create(true).append(true).open(&path) {
                Ok(file) => return (path, Some(file)),
                Err(error) => {
                    eprintln!("Antumbra: couldn't open log file '{}': {error}", path.display());
                }
            }
        }
    }

    // Best-effort fallback for unusual environments where the user data directory
    // is unavailable or not writable. Report the exact path so the user can find it.
    let fallback = std::env::current_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(LOG_FILE_NAME);
    match File::options().create(true).append(true).open(&fallback) {
        Ok(file) => (fallback, Some(file)),
        Err(error) => {
            eprintln!("Antumbra: file logging disabled; couldn't open '{}': {error}", fallback.display());
            (fallback, None)
        }
    }
}

pub fn init_logger(tui_mode: bool, verbose: bool) {
    let mut builder = env_logger::Builder::new();
    let (path, file) = open_log_file();
    let log_file = file.map(|file| Arc::new(Mutex::new(file)));
    let session_started = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z").to_string();

    // Write a session marker immediately. This distinguishes separate launches and
    // makes it obvious which file contains the logs for a reflash/relaunch.
    if let Some(ref file) = log_file {
        match file.lock() {
            Ok(mut file) => {
                let _ = writeln!(file, "\n========== Antumbra session started: {session_started} ==========");
                let _ = writeln!(file, "Log file: {}", path.display());
            }
            Err(_) => eprintln!("Antumbra: log file lock was poisoned during startup"),
        }
    }

    let formatter_log_path = path.clone();
    builder.format(move |buf: &mut Formatter, record: &Record| {
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f %:z");
        if let Some(ref log_file) = log_file {
            // A failed disk write must not suppress the same record from the UI/console.
            // Recover a poisoned mutex rather than panicking inside the global logger.
            let mut file = log_file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(error) = writeln!(file, "[{timestamp}] [{}] {}", record.level(), record.args()) {
                eprintln!("Antumbra: failed writing log file '{}': {error}", formatter_log_path.display());
            }
        }

        if tui_mode || record.level() == Level::Debug {
            Ok(())
        } else {
            let prefix = LOGGER_PREIX.bold().purple();
            let message = match record.level() {
                Level::Info => format!("{}  {}", INFO_SYMBOL.purple(), record.args()).white(),
                Level::Warn => format!("{}  {}", WARN_SYMBOL.yellow(), record.args()).yellow(),
                Level::Error => format!("{}  {}", ERROR_SYMBOL.red(), record.args()).red().bold(),
                _ => return Ok(()),
            };

            writeln!(buf, "{} {}", prefix, message)
        }
    });

    builder.filter_level(if verbose { LevelFilter::Debug } else { LevelFilter::Info });
    builder.filter_module("nusb", LevelFilter::Off); // Annoying logs :D
    builder.target(env_logger::Target::Stdout);
    builder.init();

    log::info!("Session started; persistent log: {}", path.display());

    // Capture panics into the same diagnostic log before delegating to Rust's
    // normal panic hook. This helps diagnose abrupt failures during flashing.
    let previous_hook = std::panic::take_hook();
    let panic_log = log_file_for_panic();
    std::panic::set_hook(Box::new(move |panic_info| {
        if let Some(ref file) = panic_log {
            if let Ok(mut file) = file.lock() {
                let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f %:z");
                let _ = writeln!(file, "[{timestamp}] [PANIC] {panic_info}");
                let _ = file.flush();
            }
        }
        previous_hook(panic_info);
    }));
}

fn log_file_for_panic() -> Option<Arc<Mutex<File>>> {
    let path = log_path();
    File::options().create(true).append(true).open(path).ok().map(|file| Arc::new(Mutex::new(file)))
}
