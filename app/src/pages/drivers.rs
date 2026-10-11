/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2026 Shomy
*/

use std::path::PathBuf;
use std::process::Command;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{AppCtx, AppPage};
use crate::pages::Page;

#[derive(Clone, Debug, Default)]
struct DriverStatus {
    winusb_available: bool,
    connected_mediatek: Vec<String>,
    connected_winusb: Vec<String>,
    details: String,
}

pub struct DriversPage {
    status: DriverStatus,
    message: String,
}

impl DriversPage {
    pub fn new() -> Self {
        let mut page = Self {
            status: DriverStatus::default(),
            message: String::from("Press R to scan driver status."),
        };
        page.scan();
        page
    }

    fn scan(&mut self) {
        #[cfg(target_os = "windows")]
        {
            self.status = Self::scan_windows();
            self.message = if self.status.connected_mediatek.is_empty() {
                "No MediaTek USB device detected. Connect the phone in the desired USB mode, then rescan.".into()
            } else if !self.status.connected_winusb.is_empty() {
                "A connected MediaTek device is using WinUSB.".into()
            } else {
                "MediaTek device detected, but WinUSB is not bound to it.".into()
            };
        }

        #[cfg(not(target_os = "windows"))]
        {
            self.status = DriverStatus::default();
            self.message = "Driver management is available only on Windows.".into();
        }
    }

    #[cfg(target_os = "windows")]
    fn scan_windows() -> DriverStatus {
        // Use PowerShell's Plug-and-Play properties instead of relying on device names,
        // which vary by Windows version, USB mode, and installed driver.
        let script = r#"
$ErrorActionPreference = 'SilentlyContinue'
$winusb = Test-Path "$env:windir\System32\drivers\WinUSB.sys"
$devices = Get-PnpDevice -PresentOnly | Where-Object {
    $_.InstanceId -match 'VID_0E8D' -or $_.FriendlyName -match 'MediaTek|PreLoader|BROM'
}
$result = foreach ($d in $devices) {
    $service = (Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName 'DEVPKEY_Device_Service').Data
    [pscustomobject]@{
        Name = $d.FriendlyName
        Id = $d.InstanceId
        Service = [string]$service
    }
}
[pscustomobject]@{ WinUsbAvailable = $winusb; Devices = @($result) } | ConvertTo-Json -Depth 5 -Compress
"#;

        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
            .output();

        let Ok(output) = output else {
            return DriverStatus {
                details: "Could not start PowerShell to query Plug and Play.".into(),
                ..DriverStatus::default()
            };
        };

        if !output.status.success() {
            return DriverStatus {
                details: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                ..DriverStatus::default()
            };
        }

        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
            return DriverStatus {
                details: "Windows returned an unreadable driver-status response.".into(),
                ..DriverStatus::default()
            };
        };

        let winusb_available = json["WinUsbAvailable"].as_bool().unwrap_or(false);
        let mut connected_mediatek = Vec::new();
        let mut connected_winusb = Vec::new();
        let devices = json["Devices"].as_array().cloned().unwrap_or_default();

        for device in devices {
            let name = device["Name"].as_str().unwrap_or("Unknown MediaTek device").to_string();
            let id = device["Id"].as_str().unwrap_or_default();
            let service = device["Service"].as_str().unwrap_or_default();
            let label = if id.is_empty() { name.clone() } else { format!("{name} ({id})") };
            connected_mediatek.push(label.clone());
            if service.eq_ignore_ascii_case("WinUSB") {
                connected_winusb.push(label);
            }
        }

        DriverStatus {
            winusb_available,
            connected_mediatek,
            connected_winusb,
            details: String::new(),
        }
    }

    fn open_zadig(&mut self) {
        #[cfg(target_os = "windows")]
        {
            let candidates = Self::app_directory()
                .map(|dir| vec![dir.join("zadig.exe"), dir.join("tools").join("zadig.exe")])
                .unwrap_or_default();
            if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
                match Command::new(path).spawn() {
                    Ok(_) => self.message = "Zadig launched. Select the exact MediaTek USB device and choose WinUSB; double-check the device before applying.".into(),
                    Err(error) => self.message = format!("Could not launch Zadig: {error}"),
                }
            } else {
                self.message = "Zadig is not bundled with this build yet. No driver was changed. Place the licensed Zadig executable beside Antumbra or in tools/zadig.exe.".into();
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            self.message = "Zadig is available only on Windows.".into();
        }
    }

    #[cfg(target_os = "windows")]
    fn app_directory() -> Option<PathBuf> {
        std::env::current_exe().ok()?.parent().map(PathBuf::from)
    }

    #[cfg(not(target_os = "windows"))]
    fn app_directory() -> Option<PathBuf> {
        None
    }
}

impl Page for DriversPage {
    fn render(&mut self, frame: &mut Frame<'_>, ctx: &mut AppCtx) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(2),
        ]).areas(frame.area());

        frame.render_widget(
            Block::default().title(" USB DRIVER MANAGER ").borders(Borders::ALL)
                .border_style(ctx.theme.style_accent()),
            header,
        );

        let mut lines = vec![
            Line::from(vec![
                Span::styled("Windows WinUSB component: ", ctx.theme.style_muted()),
                Span::styled(
                    if self.status.winusb_available { "AVAILABLE" } else { "NOT FOUND / CHECK FAILED" },
                    Style::default().fg(if self.status.winusb_available { ctx.theme.success } else { ctx.theme.error }).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Connected MediaTek devices: ", ctx.theme.style_muted()),
                Span::styled(self.status.connected_mediatek.len().to_string(), Style::default().fg(ctx.theme.text).add_modifier(Modifier::BOLD)),
            ]),
        ];

        if self.status.connected_mediatek.is_empty() {
            lines.push(Line::from(Span::styled("  No matching device detected.", Style::default().fg(ctx.theme.warning))));
        } else {
            for device in &self.status.connected_mediatek {
                let using_winusb = self.status.connected_winusb.iter().any(|item| item == device);
                lines.push(Line::from(vec![
                    Span::raw("  • "),
                    Span::styled(device.clone(), Style::default().fg(ctx.theme.text)),
                    Span::raw(" — "),
                    Span::styled(
                        if using_winusb { "WinUSB bound" } else { "WinUSB not bound" },
                        Style::default().fg(if using_winusb { ctx.theme.success } else { ctx.theme.warning }),
                    ),
                ]));
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Note: Windows having WinUSB.sys does not mean it is bound to the connected phone.",
            ctx.theme.style_muted(),
        )));
        if !self.status.details.is_empty() {
            lines.push(Line::from(Span::styled(self.status.details.clone(), Style::default().fg(ctx.theme.error))));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(self.message.as_str()));

        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title(" Driver status ")),
            body,
        );

        frame.render_widget(
            Paragraph::new("[R] Rescan   [I] Install / Reinstall via Zadig   [Esc] Back")
                .alignment(Alignment::Center)
                .style(ctx.theme.style_muted()),
            footer,
        );
    }

    fn handle_input(&mut self, ctx: &mut AppCtx, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => ctx.change_page(AppPage::Welcome),
            KeyCode::Char('r') | KeyCode::Char('R') => self.scan(),
            KeyCode::Char('i') | KeyCode::Char('I') => self.open_zadig(),
            _ => {}
        }
    }
}
