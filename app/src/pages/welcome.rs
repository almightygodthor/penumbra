/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2025-2026 Shomy
*/

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::app::{AppCtx, AppPage};
use crate::components::layout::{MainLayout, RectExt};
use crate::components::{Component, DescriptionMenu, Stars};
use crate::pages::{LOGO, LOGO_ASCII, Page};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    EnterDaMode,
    Options,
    Quit,
}

pub struct WelcomePage {
    menu: DescriptionMenu<MenuAction>,
    stars: Stars,
}

impl WelcomePage {
    pub fn new() -> Self {
        let menu = description_menu![
            ('*', "Enter DA Mode", "Connect to the GT Neo 3 using the bundled DA and auth." => MenuAction::EnterDaMode),
            ('*', "Options", "Configure Antumbra settings." => MenuAction::Options),
            ('*', "Quit", "Exit Antumbra." => MenuAction::Quit),
        ];

        Self { menu, stars: Stars::default() }
    }

    fn handle_menu_selection(&mut self, ctx: &mut AppCtx) {
        match self.menu.selected_action() {
            Some(MenuAction::EnterDaMode) => {
                ctx.change_page(AppPage::DevicePage);
            }
            Some(MenuAction::Options) => {
                ctx.change_page(AppPage::Options);
            }
            Some(MenuAction::Quit) => ctx.quit(),
            None => {}
        }
    }
}

impl Page for WelcomePage {
    fn update(&mut self, ctx: &mut AppCtx) {
        self.stars.tick(ctx);
    }

    fn render(&mut self, frame: &mut Frame<'_>, ctx: &mut AppCtx) {
        let area = frame.area();
        let buf = frame.buffer_mut();

        if ctx.config.tui.show_stars {
            self.stars.compact = ctx.config.tui.compatibility_mode;
            self.stars.render(area, buf, &ctx.theme);
        }

        let banner = if ctx.config.tui.compatibility_mode { LOGO_ASCII } else { LOGO };

        MainLayout::new().with_footer("[↑/↓] Navigate   •   [Enter] Select").render(
            area,
            buf,
            &ctx.theme,
            |content_area, buf| {
                // Group Logo, Menu, and Files together with fixed gaps,
                // using equal top/bottom Fill(1) to center the entire group.
                let [_, logo_block, _, menu_block, _, files_block, _] = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(8), // Logo
                    Constraint::Length(1),
                    Constraint::Length(7), // Menu
                    Constraint::Length(1),
                    Constraint::Length(3), // Files
                    Constraint::Fill(1),
                ])
                .areas(content_area);

                let logo = Paragraph::new(banner)
                    .alignment(Alignment::Center)
                    .style(ctx.theme.style_accent());
                Widget::render(logo, logo_block, buf);

                let menu_area = menu_block.centered_fixed(80, 7);
                self.menu.render(menu_area, buf, &ctx.theme);

                let files_indicator = Paragraph::new(Line::from(vec![
                    Span::styled("GT Neo 3 recovery: ", ctx.theme.style_muted()),
                    Span::styled("Bundled DA + Auth", Style::default().fg(ctx.theme.success)),
                ]))
                .alignment(Alignment::Center);
                Widget::render(files_indicator, files_block, buf);
            },
        );
    }

    fn handle_input(&mut self, ctx: &mut AppCtx, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.menu.previous(),
            KeyCode::Down | KeyCode::Char('j') => self.menu.next(),
            KeyCode::Enter => self.handle_menu_selection(ctx),
            _ => {}
        }
    }
}
