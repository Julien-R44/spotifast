//! Listening measured here, with calendar-month navigation and local rankings.

use egui::{Align, Frame, Layout, Margin, Sense, Stroke, pos2, vec2};
use jiff::{ToSpan, civil::Date};

use crate::app::App;
use crate::i18n::gettext;
use crate::model::{Action, Page};
use crate::stats::{Day, Period, Rank};
use crate::theme::{self, Icon, Palette};

use super::widgets;

#[derive(Clone, Copy, Default, PartialEq)]
enum Ranking {
    #[default]
    Tracks,
    Albums,
    Artists,
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let locale = app.locale;
    ui.add_space(12.0);
    theme::text(
        ui,
        gettext(locale, "Listening stats"),
        theme::bold(30.0),
        palette.text,
    );
    ui.add_space(4.0);
    ui.label(egui::RichText::new(gettext(
        locale,
        "Measured on this computer with the built-in player. No phone plays or estimated song durations.",
    )).color(palette.secondary));
    ui.add_space(16.0);
    periods(app, ui);
    ui.add_space(18.0);

    if let Some(error) = &app.stats.error {
        ui.label(egui::RichText::new(error).color(palette.accent));
        if theme::pill_button(ui, &palette, &gettext(locale, "Retry"), false).clicked() {
            app.actions.push(Action::Reload(Page::Stats));
        }
        ui.add_space(12.0);
    }
    let Some(report) = &app.stats.report else {
        if app.stats.loading {
            widgets::loading_row(ui, &palette, locale);
        }
        return;
    };
    if report.listened_ms == 0 {
        widgets::empty_state(
            ui,
            &palette,
            Icon::Headphones,
            &gettext(locale, "No listening recorded in this period"),
            &gettext(
                locale,
                "Play music with the built-in player to start your local history. Older Spotify plays are not imported.",
            ),
        );
        return;
    }

    let metrics = [
        (
            gettext(locale, "Time listened"),
            duration(report.listened_ms),
        ),
        (gettext(locale, "Counted plays"), report.plays.to_string()),
        (
            gettext(locale, "Different tracks"),
            report.unique_tracks.to_string(),
        ),
        (
            gettext(locale, "Different artists"),
            report.unique_artists.to_string(),
        ),
    ];
    let columns = if ui.available_width() >= 600.0 { 4 } else { 2 };
    for row in metrics.chunks(columns) {
        ui.columns(columns, |uis| {
            for (ui, (label, value)) in uis.iter_mut().zip(row) {
                Frame::new()
                    .fill(palette.surface)
                    .corner_radius(10)
                    .inner_margin(Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_min_width((ui.available_width() - 1.0).max(0.0));
                        theme::subtle(ui, &palette, label);
                        ui.add_space(6.0);
                        theme::text(ui, value, theme::bold(25.0), palette.text);
                    });
            }
        });
        ui.add_space(10.0);
    }
    ui.add_space(10.0);
    let daily_label = match app.stats.period {
        Period::Month(_) => gettext(locale, "Daily listening"),
        Period::AllTime => gettext(locale, "Daily listening · last 31 days"),
    };
    theme::section_title(ui, &palette, &daily_label);
    ui.add_space(8.0);
    daily(ui, &palette, app.stats.period, &report.days);
    ui.add_space(24.0);

    let ranking_id = ui.make_persistent_id("stats-ranking");
    let mut ranking = ui
        .data(|data| data.get_temp::<Ranking>(ranking_id))
        .unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        for (value, label) in [
            (Ranking::Tracks, gettext(locale, "Tracks")),
            (Ranking::Albums, gettext(locale, "Albums")),
            (Ranking::Artists, gettext(locale, "Artists")),
        ] {
            ui.selectable_value(&mut ranking, value, label);
        }
    });
    ui.data_mut(|data| data.insert_temp(ranking_id, ranking));
    ui.add_space(10.0);
    let rows = match ranking {
        Ranking::Tracks => &report.tracks,
        Ranking::Albums => &report.albums,
        Ranking::Artists => &report.artists,
    };
    ui.label(egui::RichText::new(gettext(locale,
        "Top 50, ordered by counted plays, then listening time. A play counts after 30 seconds or half a shorter track.",
    )).small().color(palette.secondary));
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        theme::subtle(ui, &palette, &gettext(locale, "Ranking"));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            theme::subtle(ui, &palette, &gettext(locale, "Plays · time listened"));
        });
    });
    for (index, row) in rows.iter().enumerate() {
        rank_row(ui, &palette, index, row);
    }
    if ranking == Ranking::Artists {
        ui.add_space(10.0);
        ui.label(egui::RichText::new(gettext(locale,
            "Each credited artist receives the track's listening time. Artist totals are not added together.",
        )).small().color(palette.secondary));
    }
    ui.add_space(16.0);
    ui.label(egui::RichText::new(gettext(locale,
        "Stored only in listening.sqlite3, separately for each account. Clearing Spotify caches or Recents does not delete these stats.",
    )).small().color(palette.secondary));
}

fn periods(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let locale = app.locale;
    let current = jiff::Zoned::now().date().first_of_month();
    let period = app.stats.period;
    ui.horizontal_wrapped(|ui| {
        if theme::pill_button(
            ui,
            &palette,
            &gettext(locale, "This month"),
            period == Period::Month(current),
        )
        .clicked()
        {
            app.actions
                .push(Action::SetStatsPeriod(Period::Month(current)));
        }
        if theme::pill_button(
            ui,
            &palette,
            &gettext(locale, "All time"),
            period == Period::AllTime,
        )
        .clicked()
        {
            app.actions.push(Action::SetStatsPeriod(Period::AllTime));
        }
        if let Period::Month(month) = period {
            if theme::icon_button(
                ui,
                Icon::ChevronLeft,
                18.0,
                palette.secondary,
                palette.text,
                &gettext(locale, "Previous month"),
            )
            .clicked()
                && let Ok(previous) = month.checked_sub(1.month())
            {
                app.actions
                    .push(Action::SetStatsPeriod(Period::Month(previous)));
            }
            theme::text(
                ui,
                month.strftime("%Y-%m").to_string(),
                theme::bold(15.0),
                palette.text,
            );
            ui.add_enabled_ui(month < current, |ui| {
                if theme::icon_button(
                    ui,
                    Icon::ChevronRight,
                    18.0,
                    palette.secondary,
                    palette.text,
                    &gettext(locale, "Next month"),
                )
                .clicked()
                    && let Ok(next) = month.checked_add(1.month())
                {
                    app.actions
                        .push(Action::SetStatsPeriod(Period::Month(next)));
                }
            });
        }
    });
}

fn daily(ui: &mut egui::Ui, palette: &Palette, period: Period, days: &[Day]) {
    let today = jiff::Zoned::now().date();
    let (first, count) = match period {
        Period::Month(month) => (month, month.days_in_month() as usize),
        Period::AllTime => (today.checked_sub(30.days()).unwrap_or(today), 31),
    };
    let values: Vec<(Date, u64)> = (0..count)
        .filter_map(|index| {
            let date = first.checked_add((index as i64).days()).ok()?;
            Some((
                date,
                days.iter()
                    .find(|day| day.date == date)
                    .map_or(0, |day| day.listened_ms),
            ))
        })
        .collect();
    let max = values.iter().map(|(_, ms)| *ms).max().unwrap_or(1).max(1);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, 125.0), Sense::hover());
    let step = rect.width() / count as f32;
    let base = rect.bottom() - 22.0;
    ui.painter()
        .hline(rect.x_range(), base, Stroke::new(1.0, palette.outline));
    for (index, (date, ms)) in values.iter().enumerate() {
        let left = rect.left() + index as f32 * step;
        let height = 94.0 * *ms as f32 / max as f32;
        let bar = egui::Rect::from_min_max(
            pos2(left + 2.0, base - height.max(2.0)),
            pos2(left + step - 2.0, base),
        );
        let hit = egui::Rect::from_min_max(pos2(left, rect.top()), pos2(left + step, base));
        let response = ui.interact(hit, ui.id().with(("stats-day", index)), Sense::hover());
        let label = format!("{} · {}", date, duration(*ms));
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
        ui.painter().rect_filled(
            bar,
            3,
            if *ms == 0 {
                palette.outline
            } else if response.hovered() {
                palette.accent_hover
            } else {
                palette.accent
            },
        );
        response.on_hover_text(label);
        if index == 0 || index == count - 1 || date.day() % 7 == 0 {
            ui.painter().text(
                pos2(left + step / 2.0, base + 8.0),
                egui::Align2::CENTER_TOP,
                date.day().to_string(),
                theme::regular(11.0),
                palette.secondary,
            );
        }
    }
}

fn rank_row(ui: &mut egui::Ui, palette: &Palette, index: usize, row: &Rank) {
    let width = ui.available_width();
    Frame::new()
        .fill(if index.is_multiple_of(2) {
            palette.surface
        } else {
            egui::Color32::TRANSPARENT
        })
        .corner_radius(6)
        .inner_margin(Margin::symmetric(10, 9))
        .show(ui, |ui| {
            ui.set_min_width((width - 20.0).max(0.0));
            ui.horizontal(|ui| {
                ui.allocate_ui(vec2(25.0, 30.0), |ui| {
                    theme::text(
                        ui,
                        (index + 1).to_string(),
                        theme::regular(13.0),
                        palette.secondary,
                    );
                });
                let names_width = (ui.available_width() - 128.0).max(30.0);
                ui.allocate_ui(vec2(names_width, 34.0), |ui| {
                    theme::text(ui, &row.name, theme::bold(14.0), palette.text)
                        .on_hover_text(&row.name);
                    if !row.subtitle.is_empty() {
                        theme::subtle(ui, palette, &row.subtitle).on_hover_text(&row.subtitle);
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    theme::text(
                        ui,
                        format!("{} · {}", row.plays, duration(row.listened_ms)),
                        theme::regular(13.0),
                        palette.secondary,
                    );
                });
            });
        });
    ui.add_space(2.0);
}

fn duration(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, seconds / 60 % 60)
    }
}
