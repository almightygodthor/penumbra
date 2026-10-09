/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2026 Shomy
*/

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use penumbra::activity::DeviceActivity;
use penumbra::port::PortType;
use penumbra::{Device, Partition};

use super::worker::{DeviceCommand, DeviceEvent, ScatterReviewItem};
use crate::components::ActivityExt;
use crate::helpers::ScatterFiles;

pub struct DeviceIo<'a> {
    event_tx: &'a Sender<DeviceEvent>,
    cmd_rx: &'a Receiver<DeviceCommand>,
    partitions: &'a [Partition],
    activity: &'a DeviceActivity,
}

impl<'a> DeviceIo<'a> {
    pub const fn new(
        event_tx: &'a Sender<DeviceEvent>,
        cmd_rx: &'a Receiver<DeviceCommand>,
        partitions: &'a [Partition],
        activity: &'a DeviceActivity,
    ) -> Self {
        Self { event_tx, cmd_rx, partitions, activity }
    }

    pub fn activity_handle(&self) -> DeviceActivity {
        self.activity.clone()
    }

    pub fn status(&self, message: impl Into<String>) {
        let _ = self.event_tx.send(DeviceEvent::HeaderStatus(message.into()));
    }

    pub fn progress_start(&self, total_bytes: u64, message: impl Into<String>) {
        let _ =
            self.event_tx.send(DeviceEvent::ProgressStart { total_bytes, message: message.into() });
    }

    pub fn progress(&self, written: u64, message: Option<String>) {
        let _ = self.event_tx.send(DeviceEvent::ProgressUpdate { written, total: None, message });
    }

    pub fn progress_finish(&self, message: impl Into<String>) {
        let _ = self.event_tx.send(DeviceEvent::ProgressFinish { message: message.into() });
    }

    pub fn progress_reporter(&self) -> ProgressReporter {
        ProgressReporter { event_tx: self.event_tx.clone() }
    }

    /// Asks the UI to let the user pick partitions for the action.
    pub fn ask_partitions(&self) -> Option<Vec<Partition>> {
        let _ = self.event_tx.send(DeviceEvent::NeedPartitions);
        loop {
            match self.cmd_rx.recv() {
                Ok(DeviceCommand::PartitionsChosen(names)) if !names.is_empty() => {
                    let resolved: Vec<Partition> = self
                        .partitions
                        .iter()
                        .filter(|p| names.iter().any(|n| n == &p.name))
                        .cloned()
                        .collect();
                    if resolved.is_empty() {
                        continue;
                    }
                    return Some(resolved);
                }
                Ok(DeviceCommand::Cancel) | Err(_) => return None,
                _ => {}
            }
        }
    }

    pub fn get_image_size(path: &Path, partition: &Partition) -> anyhow::Result<u64> {
        let size = std::fs::metadata(path)?.len();

        if size > partition.size { Ok(partition.size) } else { Ok(size) }
    }

    /// Asks the UI to open the file explorer to let the user pick a file or directory.
    pub fn ask_scatter_selection(&self, items: Vec<ScatterReviewItem>) -> Option<Vec<String>> {
        let _ = self.event_tx.send(DeviceEvent::NeedScatterFiles(items));
        loop {
            match self.cmd_rx.recv() {
                Ok(DeviceCommand::PartitionsChosen(names)) if !names.is_empty() => return Some(names),
                Ok(DeviceCommand::Cancel) | Err(_) => return None,
                _ => {}
            }
        }
    }

    pub fn ask_file(
        &self,
        title: impl Into<String>,
        directories_only: bool,
        extensions: Option<Vec<&'static str>>,
    ) -> Option<PathBuf> {
        let _ = self.event_tx.send(DeviceEvent::NeedFile {
            title: title.into(),
            directories_only,
            extensions,
        });
        loop {
            match self.cmd_rx.recv() {
                Ok(DeviceCommand::FileChosen(path)) => return Some(path),
                Ok(DeviceCommand::Cancel) | Err(_) => return None,
                _ => {}
            }
        }
    }
}

#[derive(Clone)]
pub struct ProgressReporter {
    event_tx: Sender<DeviceEvent>,
}

impl ProgressReporter {
    pub fn update(&self, written: u64, message: Option<String>) {
        let _ = self.event_tx.send(DeviceEvent::ProgressUpdate { written, total: None, message });
    }
}

pub trait DeviceAction: Send + Sync {
    fn label(&self) -> &'static str;
    fn run(&self, dev: &mut Device<'_, PortType>, io: &DeviceIo<'_>) -> anyhow::Result<bool>;
    fn changes_layout(&self) -> bool {
        false
    }
}

pub fn actions() -> Vec<Box<dyn DeviceAction>> {
    vec![
        Box::new(ReadPartition),
        Box::new(WritePartition),
        Box::new(ErasePartition),
        Box::new(DumpAllPartitions),
        Box::new(WriteAllPartitions),
        Box::new(FlashScatter),
        Box::new(BackupImei),
    ]
}

pub struct ReadPartition;

impl DeviceAction for ReadPartition {
    fn label(&self) -> &'static str {
        "Read Partition"
    }

    fn run(&self, dev: &mut Device<'_, PortType>, io: &DeviceIo<'_>) -> anyhow::Result<bool> {
        let Some(partitions) = io.ask_partitions() else { return Ok(false) };
        let Some(output_dir) = io.ask_file("Output dump directory", true, None) else {
            return Ok(false);
        };

        let total_bytes: u64 = partitions.iter().map(|p| p.size).sum();
        let mut bytes_done: u64 = 0;

        io.progress_start(total_bytes, "Reading partitions...");
        let reporter = io.progress_reporter();

        for partition in &partitions {
            let output_path = output_dir.join(format!("{}.bin", partition.name));
            let file = File::create(&output_path)?;
            let mut writer = BufWriter::new(file);
            let name = partition.name.clone();
            let reporter = reporter.clone();

            dev.read_partition(&partition.name, &mut writer, move |written, _total| {
                reporter.update(bytes_done + written, Some(format!("Reading '{name}'...")));
            })?;

            bytes_done += partition.size;
        }

        io.progress_finish("Partition read complete.");
        Ok(true)
    }
}

pub struct WritePartition;

impl DeviceAction for WritePartition {
    fn changes_layout(&self) -> bool {
        true
    }

    fn label(&self) -> &'static str {
        "Write Partition"
    }

    fn run(&self, dev: &mut Device<'_, PortType>, io: &DeviceIo<'_>) -> anyhow::Result<bool> {
        let Some(partitions) = io.ask_partitions() else { return Ok(false) };

        // user first selects partitions, then for each one of them we ask for the specific file to
        // write.
        let mut to_write: Vec<(Partition, PathBuf, u64)> = Vec::with_capacity(partitions.len());
        for partition in partitions {
            let title = format!("Select file for partition '{}'", partition.name);
            let Some(path) = io.ask_file(title, false, None) else { return Ok(false) };

            let size = DeviceIo::get_image_size(&path, &partition)?;
            to_write.push((partition, path, size));
        }

        let total_bytes: u64 = to_write.iter().map(|(.., size)| size).sum();
        let mut bytes_done: u64 = 0;

        io.progress_start(total_bytes, "Writing partitions...");
        let reporter = io.progress_reporter();

        for (partition, path, size) in &to_write {
            let file = File::open(path)?;
            let mut reader = BufReader::new(file);
            let name = partition.name.clone();
            let reporter = reporter.clone();

            dev.write_partition(&partition.name, *size, &mut reader, move |written, _total| {
                reporter.update(bytes_done + written, Some(format!("Flashing '{name}'...")));
            })?;

            bytes_done += size;
        }

        io.progress_finish("Partition write complete.");
        Ok(true)
    }
}

pub struct ErasePartition;

impl DeviceAction for ErasePartition {
    fn changes_layout(&self) -> bool {
        true
    }

    fn label(&self) -> &'static str {
        "Erase Partition"
    }

    fn run(&self, dev: &mut Device<'_, PortType>, io: &DeviceIo<'_>) -> anyhow::Result<bool> {
        let Some(partitions) = io.ask_partitions() else { return Ok(false) };

        let total_bytes: u64 = partitions.iter().map(|p| p.size).sum();
        let mut bytes_done: u64 = 0;

        io.progress_start(total_bytes, "Erasing partitions...");
        let reporter = io.progress_reporter();

        for partition in &partitions {
            let name = partition.name.clone();
            let reporter = reporter.clone();

            dev.erase_partition(&partition.name, move |written, _total| {
                reporter.update(bytes_done + written, Some(format!("Erasing '{name}'...")));
            })?;

            bytes_done += partition.size;
        }

        io.progress_finish("Partition erase complete.");
        Ok(true)
    }
}

pub struct DumpAllPartitions;

impl DeviceAction for DumpAllPartitions {
    fn label(&self) -> &'static str {
        "Dump all partitions"
    }

    fn run(&self, dev: &mut Device<'_, PortType>, io: &DeviceIo<'_>) -> anyhow::Result<bool> {
        let partitions: Vec<Partition> =
            dev.partitions_iter().filter(|p| p.name != "userdata").collect();

        let Some(output_dir) = io.ask_file("Output dump directory", true, None) else {
            return Ok(false);
        };

        // We skip userdata since it's too big and not something people usually want to dump.
        // If someone really wants to do it, there's always the "Read Partition" action.
        let total_bytes: u64 = partitions.iter().map(|p| p.size).sum();
        let mut bytes_done: u64 = 0;

        io.progress_start(total_bytes, "Dumping all partitions...");
        let reporter = io.progress_reporter();

        for partition in &partitions {
            let output_path = output_dir.join(format!("{}.bin", partition.name));
            let file = File::create(&output_path)?;
            let mut writer = BufWriter::new(file);
            let name = partition.name.clone();
            let reporter = reporter.clone();

            dev.read_partition(&partition.name, &mut writer, move |written, _total| {
                reporter.update(bytes_done + written, Some(format!("Dumping '{name}'...")));
            })?;

            bytes_done += partition.size;
        }

        io.progress_finish("All partitions dumped.");
        Ok(true)
    }
}

pub struct WriteAllPartitions;

impl DeviceAction for WriteAllPartitions {
    fn changes_layout(&self) -> bool {
        true
    }

    fn label(&self) -> &'static str {
        "Write all partitions"
    }

    fn run(&self, dev: &mut Device<'_, PortType>, io: &DeviceIo<'_>) -> anyhow::Result<bool> {
        let partitions = dev.partitions();

        let Some(input_dir) = io.ask_file("Input dump directory", true, None) else {
            return Ok(false);
        };

        let mut to_write: Vec<(Partition, PathBuf, u64)> = Vec::new();

        for partition in partitions {
            let input_path = input_dir.join(format!("{}.bin", partition.name));

            if !input_path.exists() {
                io.status(format!(
                    "Skipping partition '{}' because file '{}' does not exist.",
                    partition.name,
                    input_path.display()
                ));
                continue;
            }

            let size = DeviceIo::get_image_size(&input_path, &partition)?;
            to_write.push((partition, input_path, size));
        }

        let total_bytes: u64 = to_write.iter().map(|(.., size)| size).sum();
        let mut bytes_done: u64 = 0;

        io.progress_start(total_bytes, "Writing all partitions...");
        let reporter = io.progress_reporter();

        for (partition, input_path, size) in &to_write {
            let file = File::open(input_path)?;
            let mut reader = BufReader::new(file);
            let name = partition.name.clone();
            let reporter = reporter.clone();

            dev.write_partition(&partition.name, *size, &mut reader, move |written, _total| {
                reporter.update(bytes_done + written, Some(format!("Flashing '{name}'...")));
            })?;

            bytes_done += size;
        }

        io.progress_finish("All partitions written.");
        Ok(true)
    }
}

/// Backs up partitions commonly associated with modem identity and calibration.
pub struct BackupImei;

impl DeviceAction for BackupImei {
    fn label(&self) -> &'static str {
        "Backup IMEI partitions"
    }

    fn run(&self, dev: &mut Device<'_, PortType>, io: &DeviceIo<'_>) -> anyhow::Result<bool> {
        const PARTITIONS: &[&str] = &[
            "nvram",
            "nvdata",
            "persist",
            "nvcfg",
            "ocdt",
            "oplus_custom",
            "oplusreserve1",
        ];

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let output_dir = std::env::current_dir()?.join(format!("IMEI_Backup_{stamp}"));
        std::fs::create_dir_all(&output_dir)?;

        let available: Vec<Partition> = dev.partitions_iter().collect();
        let present: Vec<&str> = PARTITIONS.iter().copied()
            .filter(|name| available.iter().any(|partition| partition.name == *name))
            .collect();
        let missing: Vec<&str> = PARTITIONS.iter().copied()
            .filter(|name| !available.iter().any(|partition| partition.name == *name))
            .collect();

        if present.is_empty() {
            anyhow::bail!("None of the requested IMEI-related partitions were found on this device.");
        }

        io.status(format!("IMEI backup folder: {}", output_dir.display()));
        log::info!("Starting IMEI-related partition backup.");
        log::info!("Backup destination: {}", output_dir.display());
        for name in &missing {
            log::warn!("IMEI backup: partition '{}' not present; skipped.", name);
        }

        let total_bytes: u64 = available.iter()
            .filter(|partition| present.contains(&partition.name.as_str()))
            .map(|partition| partition.size)
            .sum();
        io.progress_start(total_bytes, "Backing up IMEI-related partitions...");
        let reporter = io.progress_reporter();
        let mut bytes_done = 0_u64;
        let mut backed_up = Vec::new();

        for name in &present {
            let partition = available.iter().find(|partition| partition.name == *name).unwrap();
            let path = output_dir.join(format!("{name}.img"));
            io.status(format!("Backing up {name}..."));
            log::info!("Reading partition '{}' -> '{}'", name, path.display());

            let file = File::create(&path)?;
            let mut writer = BufWriter::new(file);
            let partition_name = (*name).to_string();
            let progress = reporter.clone();
            let base = bytes_done;

            if let Err(error) = dev.read_partition(name, &mut writer, move |read, _total| {
                progress.update(base + read, Some(format!("Backing up {partition_name}...")));
            }) {
                log::error!("IMEI backup failed for '{}': {error:#}", name);
                io.status(format!("Backup failed for {name}: {error}"));
                return Err(error.into());
            }

            bytes_done += partition.size;
            backed_up.push((*name).to_string());
            log::info!("[BACKUP OK] {} -> {}", name, path.display());
            io.status(format!("Backed up {}/{}: {}", backed_up.len(), present.len(), name));
        }

        log::info!("========== IMEI BACKUP REPORT ==========");
        for name in &backed_up {
            log::info!("[OK] {}.img", name);
        }
        log::info!("Backed up: {}", backed_up.len());
        log::info!("Skipped (not present): {}", missing.len());
        log::info!("Backup location: {}", output_dir.display());
        io.progress_finish(format!("IMEI backup saved to {}", output_dir.display()));
        io.status(format!("IMEI backup saved: {}", output_dir.display()));
        Ok(false)
    }
}

pub struct FlashScatter;

impl FlashScatter {
    pub fn run_prepared(
        &self,
        dev: &mut Device<'_, PortType>,
        io: &DeviceIo<'_>,
        scatter: &Path,
        selected: &[String],
    ) -> anyhow::Result<bool> {
        let scatter_content = std::fs::read_to_string(scatter)?;
        let scatter_dir = scatter.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
        let items = scatter_review_items(&scatter_content, &scatter_dir);
        let selected_names: std::collections::HashSet<&str> =
            selected.iter().map(String::as_str).collect();

        let missing: Vec<String> = items
            .iter()
            .filter(|item| item.downloadable && selected_names.contains(item.name.as_str()) && !item.found)
            .map(|item| format!("{}: {}", item.name, scatter_display_path(&item.filename, &scatter_dir).display()))
            .collect();
        if !missing.is_empty() {
            anyhow::bail!("Selected partition image(s) are missing:\n{}", missing.join("\n"));
        }

        let selected_set: std::collections::HashSet<&str> = items
            .iter()
            .filter(|item| item.downloadable && item.found && selected_names.contains(item.name.as_str()))
            .map(|item| item.name.as_str())
            .collect();
        if selected_set.is_empty() {
            anyhow::bail!("No available partition images selected for flashing.");
        }

        log::info!(
            "Scatter flash prepared: scatter='{}', selected_partitions={}",
            scatter.display(),
            selected_set.len()
        );

        let filtered_scatter = filter_scatter_downloads(&scatter_content, &selected_set);
        let files = ScatterFiles::new(scatter_dir);
        let readers = files.clone();
        let reader_source = move |file_path: &str| readers.reader(file_path);
        let writer_sink = move |file_path: &str| files.writer(file_path);

        let mut started = false;
        let event_tx = io.event_tx.clone();
        let activity = io.activity_handle();
        let progress_callback = move |curr: u64, total: u64| {
            if !started {
                let _ = event_tx.send(DeviceEvent::ProgressStart {
                    total_bytes: total,
                    message: "Flashing from scatter file...".into(),
                });
                started = true;
            }
            let _ = event_tx.send(DeviceEvent::ProgressUpdate {
                written: curr,
                total: Some(total),
                message: activity.current().detail(),
            });
        };

        log::info!("Scatter flash started: '{}'", scatter.display());
        io.status(format!("Flashing {} selected partitions...", selected_set.len()));
        if let Err(error) = dev.flash_scatter(&filtered_scatter, reader_source, writer_sink, progress_callback) {
            log::error!("Scatter flash operation failed: {error:#}",);
            log::error!("Scatter flash report: {} selected; overall operation FAILED. Individual write completion cannot be confirmed by the scatter API.", selected_set.len());
            return Err(error.into());
        }

        // The scatter API reports aggregate success, not an independent read-back
        // verification for every partition. Label entries as operation-completed,
        // never as content-verified.
        log::info!("========== SCATTER FLASH REPORT ==========");
        let mut report_names: Vec<&str> = selected_set.iter().copied().collect();
        report_names.sort_unstable();
        for name in &report_names {
            log::info!("[OK] {} — write operation completed (not read-back verified)", name);
            io.status(format!("Flash report: {}/{} completed", report_names.iter().position(|n| n == name).unwrap_or(0) + 1, report_names.len()));
        }
        log::info!("Selected: {}", report_names.len());
        log::info!("Completed: {}", report_names.len());
        log::info!("Failed: 0 (scatter API returned success)");
        log::info!("Read-back verification: NOT PERFORMED");
        log::info!("Report complete.");
        log::info!("Scatter flash completed successfully: '{}'", scatter.display());
        io.progress_finish(format!("Scatter flash operation completed: {} selected; read-back verification not performed.", report_names.len()));
        Ok(true)
    }
}

impl DeviceAction for FlashScatter {
    fn changes_layout(&self) -> bool { true }
    fn label(&self) -> &'static str { "Flash from scatter file" }

    fn run(&self, dev: &mut Device<'_, PortType>, io: &DeviceIo<'_>) -> anyhow::Result<bool> {
        let Some(scatter) = io.ask_file("Scatter file", false, Some(vec!["txt", "xml"])) else {
            return Ok(false);
        };
        let items = load_scatter_review(&scatter)?;
        if items.is_empty() {
            anyhow::bail!("No partitions were found in the selected scatter file.");
        }
        let Some(selected) = io.ask_scatter_selection(items) else { return Ok(false) };
        self.run_prepared(dev, io, &scatter, &selected)
    }
}

pub fn load_scatter_review(scatter: &Path) -> anyhow::Result<Vec<ScatterReviewItem>> {
    let content = std::fs::read_to_string(scatter)?;
    let dir = scatter.parent().unwrap_or_else(|| Path::new(""));
    Ok(scatter_review_items(&content, dir))
}

fn scatter_tag(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    Some(block[start..end].trim().to_string())
}

fn scatter_display_path(filename: &str, base_dir: &Path) -> PathBuf {
    if Path::new(filename).is_absolute() {
        return PathBuf::from(filename);
    }

    let mut clean = filename.trim_start_matches("./");
    if let Some(stripped) = clean.strip_prefix("backup/") {
        clean = stripped.trim_start_matches("./");
    }
    if let Some(stripped) = clean.strip_prefix("out/") {
        clean = stripped.trim_start_matches("./");
    }
    base_dir.join(clean)
}

fn scatter_review_items(content: &str, base_dir: &Path) -> Vec<ScatterReviewItem> {
    let mut items = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("<partition_index") {
        rest = &rest[start..];
        let Some(end_rel) = rest.find("</partition_index>") else { break };
        let block = &rest[..end_rel + "</partition_index>".len()];
        rest = &rest[end_rel + "</partition_index>".len()..];

        let Some(name) = scatter_tag(block, "partition_name") else { continue };
        let filename = scatter_tag(block, "file_name").unwrap_or_else(|| "NONE".into());
        let downloadable = scatter_tag(block, "is_download").is_some_and(|v| v.eq_ignore_ascii_case("true"))
            && filename != "NONE";
        let found = if !downloadable {
            false
        } else {
            scatter_display_path(&filename, base_dir).is_file()
        };

        items.push(ScatterReviewItem { name, filename, found, downloadable });
    }
    items
}

fn filter_scatter_downloads(content: &str, selected: &std::collections::HashSet<&str>) -> String {
    let mut output = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("<partition_index") {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end_rel) = rest.find("</partition_index>") else {
            output.push_str(rest);
            return output;
        };
        let end = end_rel + "</partition_index>".len();
        let block = &rest[..end];
        let name = scatter_tag(block, "partition_name").unwrap_or_default();
        if !selected.contains(name.as_str()) {
            let mut changed = block.to_string();
            if let Some(value_start) = changed.find("<is_download>") {
                let value_start = value_start + "<is_download>".len();
                if let Some(value_end_rel) = changed[value_start..].find("</is_download>") {
                    let value_end = value_start + value_end_rel;
                    changed.replace_range(value_start..value_end, "false");
                }
            }
            output.push_str(&changed);
        } else {
            output.push_str(block);
        }
        rest = &rest[end..];
    }
    output.push_str(rest);
    output
}
