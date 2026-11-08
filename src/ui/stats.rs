//! A listening overview, daily rhythm, and artwork-led local rankings.

use egui::{Align, Frame, Layout, Margin, Rect, Sense, Stroke, UiBuilder, pos2, vec2};
use jiff::{ToSpan, civil::Date};

use crate::app::App;
use crate::i18n::{Locale, gettext, ngettext};
use crate::model::{Action, Page};
use crate::stats::{Day, Period, Rank, Report};
use crate::theme::{self, Icon, Palette};
use crate::util;

use super::widgets;

#[derive(Clone, Copy, Default, PartialEq)]
enum Ranking {
    #[default]
    Tracks,
    Albums,
    Artists,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Metric {
    #[default]
    Time,
    Plays,
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let locale = app.locale;
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        theme::icon(ui, Icon::Headphones, 15.0, palette.accent);
        theme::text(ui, gettext(locale, "Listening stats"), theme::medium(13.0), palette.secondary);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            theme::icon_button(ui, Icon::Info, 16.0, palette.secondary, palette.text,
                &gettext(locale, "Measured on this computer with the built-in player. No phone plays or estimated song durations."));
            theme::text(ui, gettext(locale, "Local history"), theme::regular(12.0), palette.secondary);
        });
    });
    ui.add_space(9.0);
    let title = match app.stats.period {
        Period::Month(month) => util::format_date(locale, &month.strftime("%Y-%m").to_string()),
        Period::AllTime => gettext(locale, "All your listening").into_owned(),
    };
    theme::text(ui, title, theme::bold(36.0), palette.text);
    ui.add_space(14.0);
    periods(app, ui);
    ui.add_space(18.0);

    if let Some(error) = &app.stats.error {
        Frame::new()
            .fill(palette.danger.gamma_multiply(0.08))
            .stroke(Stroke::new(1.0, palette.danger.gamma_multiply(0.35)))
            .corner_radius(12)
            .inner_margin(16)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                theme::text(
                    ui,
                    gettext(locale, "Couldn't load listening stats"),
                    theme::bold(16.0),
                    palette.text,
                );
                ui.label(egui::RichText::new(error).color(palette.secondary));
                ui.add_space(8.0);
                if theme::pill_button(ui, &palette, &gettext(locale, "Retry"), false).clicked() {
                    app.actions.push(Action::Reload(Page::Stats));
                }
            });
        ui.add_space(16.0);
    }
    let Some(report) = &app.stats.report else {
        if app.stats.loading {
            widgets::loading_row(ui, &palette, locale);
        }
        return;
    };
    if report.listened_ms == 0 && report.plays == 0 {
        Frame::new().fill(palette.surface).corner_radius(16).inner_margin(24).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                theme::icon(ui, Icon::Headphones, 40.0, palette.dim);
                ui.add_space(8.0);
                ui.add(egui::Label::new(egui::RichText::new(gettext(locale, "Your next listen starts the story"))
                    .font(theme::semibold(16.0)).color(palette.text)).wrap());
                ui.add_space(4.0);
                ui.add(egui::Label::new(egui::RichText::new(gettext(locale,
                    "Play music with the built-in player. Your time, favorites, and daily rhythm will appear here. Older Spotify plays are not imported."))
                    .font(theme::regular(13.5)).color(palette.secondary)).wrap());
            });
            ui.add_space(12.0);
        });
        return;
    }

    overview(ui, &palette, locale, report);
    ui.add_space(16.0);
    if ui.available_width() >= 780.0 {
        let chart_width = ui.available_width() - 258.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            ui.allocate_ui_with_layout(
                vec2(chart_width, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    rhythm(ui, &palette, locale, app.stats.period, &report.days);
                },
            );
            ui.allocate_ui_with_layout(vec2(242.0, 0.0), Layout::top_down(Align::Min), |ui| {
                insights(ui, &palette, locale, report);
            });
        });
    } else {
        rhythm(ui, &palette, locale, app.stats.period, &report.days);
        ui.add_space(12.0);
        insights(ui, &palette, locale, report);
    }
    ui.add_space(20.0);
    let action = rankings(app, ui, report);
    ui.add_space(20.0);
    ui.horizontal_wrapped(|ui| {
        theme::icon(ui, Icon::Info, 13.0, palette.dim);
        ui.label(egui::RichText::new(gettext(locale,
            "Stored only in listening.sqlite3, separately for each account. Clearing Spotify caches or Recents does not delete these stats."
        )).small().color(palette.secondary));
    });
    if let Some(action) = action {
        app.actions.push(action);
    }
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
            ui.add_space(6.0);
            if theme::icon_button(
                ui,
                Icon::ChevronLeft,
                17.0,
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
            ui.add_enabled_ui(month < current, |ui| {
                if theme::icon_button(
                    ui,
                    Icon::ChevronRight,
                    17.0,
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

fn overview(ui: &mut egui::Ui, palette: &Palette, locale: Locale, report: &Report) {
    let metrics = [
        (
            Icon::Clock,
            gettext(locale, "Time listened"),
            util::format_total_ms(locale, report.listened_ms),
            gettext(locale, "Measured, not estimated").into_owned(),
        ),
        (
            Icon::Headphones,
            gettext(locale, "Counted plays"),
            util::format_count(report.plays),
            gettext(locale, "30 seconds or half a short track").into_owned(),
        ),
        (
            Icon::Music,
            gettext(locale, "Different tracks"),
            util::format_count(report.unique_tracks),
            ngettext(
                locale,
                "{count} album",
                "{count} albums",
                report.unique_albums as u32,
            )
            .replace("{count}", &util::format_count(report.unique_albums)),
        ),
        (
            Icon::Users,
            gettext(locale, "Different artists"),
            util::format_count(report.unique_artists),
            gettext(locale, "Including collaborations").into_owned(),
        ),
    ];
    let columns = if ui.available_width() >= 740.0 { 4 } else { 2 };
    for (row_index, row) in metrics.chunks(columns).enumerate() {
        ui.columns(columns, |uis| {
            for (column, (ui, (icon, label, value, detail))) in uis.iter_mut().zip(row).enumerate()
            {
                let primary = row_index == 0 && column == 0;
                Frame::new()
                    .fill(if primary {
                        palette.surface.lerp_to_gamma(palette.accent, 0.10)
                    } else {
                        palette.surface
                    })
                    .stroke(Stroke::new(
                        1.0,
                        if primary {
                            palette.accent.gamma_multiply(0.22)
                        } else {
                            palette.outline.gamma_multiply(0.45)
                        },
                    ))
                    .corner_radius(14)
                    .inner_margin(Margin::symmetric(18, 14))
                    .show(ui, |ui| {
                        ui.set_min_width((ui.available_width() - 1.0).max(0.0));
                        ui.horizontal(|ui| {
                            theme::icon(
                                ui,
                                *icon,
                                14.0,
                                if primary {
                                    palette.accent
                                } else {
                                    palette.secondary
                                },
                            );
                            theme::text(
                                ui,
                                label.as_ref(),
                                theme::regular(12.0),
                                palette.secondary,
                            );
                        });
                        ui.add_space(8.0);
                        theme::text(ui, value, theme::bold(30.0), palette.text)
                            .on_hover_text(value);
                        ui.add_space(4.0);
                        theme::text(ui, detail, theme::regular(11.0), palette.secondary)
                            .on_hover_text(detail);
                    });
            }
        });
        if row_index + 1 < metrics.len().div_ceil(columns) {
            ui.add_space(10.0);
        }
    }
}

fn rhythm(ui: &mut egui::Ui, palette: &Palette, locale: Locale, period: Period, days: &[Day]) {
    let id = ui.make_persistent_id("stats-chart-metric");
    let mut metric = ui
        .data(|data| data.get_temp::<Metric>(id))
        .unwrap_or_default();
    Frame::new()
        .fill(palette.surface)
        .corner_radius(14)
        .inner_margin(18)
        .show(ui, |ui| {
            ui.set_min_width((ui.available_width() - 1.0).max(0.0));
            ui.horizontal(|ui| {
                theme::text(
                    ui,
                    gettext(locale, "Daily listening"),
                    theme::bold(16.0),
                    palette.text,
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.selectable_value(&mut metric, Metric::Plays, gettext(locale, "Plays"));
                    ui.selectable_value(&mut metric, Metric::Time, gettext(locale, "Time"));
                });
            });
            theme::text(
                ui,
                match period {
                    Period::Month(_) => gettext(locale, "Actual playback, day by day"),
                    Period::AllTime => {
                        gettext(locale, "Last 31 days · totals above cover all time")
                    }
                },
                theme::regular(12.0),
                palette.secondary,
            );
            ui.add_space(14.0);
            daily(ui, palette, locale, period, days, metric);
        });
    ui.data_mut(|data| data.insert_temp(id, metric));
}

fn daily(
    ui: &mut egui::Ui,
    palette: &Palette,
    locale: Locale,
    period: Period,
    days: &[Day],
    metric: Metric,
) {
    let today = jiff::Zoned::now().date();
    let (first, count) = match period {
        Period::Month(month) => (month, month.days_in_month() as usize),
        Period::AllTime => (today.checked_sub(30.days()).unwrap_or(today), 31),
    };
    let values: Vec<_> = (0..count)
        .filter_map(|index| {
            let date = first.checked_add((index as i64).days()).ok()?;
            let day = days.iter().find(|day| day.date == date);
            Some((
                date,
                day.map_or(0, |day| day.listened_ms),
                day.map_or(0, |day| day.plays),
            ))
        })
        .collect();
    let value = |ms: u64, plays: u64| if metric == Metric::Time { ms } else { plays };
    let maximum = values
        .iter()
        .map(|(_, ms, plays)| value(*ms, *plays))
        .max()
        .unwrap_or(0)
        .max(1);
    let unit = if metric == Metric::Time {
        match maximum {
            0..60_000 => 10_000,
            60_000..900_000 => 60_000,
            _ => 900_000,
        }
    } else {
        maximum.div_ceil(4).max(1)
    };
    let ceiling = maximum.div_ceil(2 * unit) * (2 * unit);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 144.0), Sense::hover());
    let plot = Rect::from_min_max(rect.min + vec2(42.0, 10.0), rect.max - vec2(0.0, 25.0));
    for fraction in [0.0, 0.5, 1.0] {
        let y = plot.bottom() - plot.height() * fraction;
        ui.painter().hline(
            plot.x_range(),
            y,
            Stroke::new(1.0, palette.outline.gamma_multiply(0.65)),
        );
        let at = (ceiling as f32 * fraction) as u64;
        let label = if metric == Metric::Time {
            if at >= 3_600_000 {
                format!("{:.1} h", at as f64 / 3_600_000.0)
            } else if at < 60_000 {
                format!("{} s", at / 1_000)
            } else {
                format!("{} min", at / 60_000)
            }
        } else {
            at.to_string()
        };
        ui.painter().text(
            pos2(plot.left() - 8.0, y),
            egui::Align2::RIGHT_CENTER,
            label,
            theme::regular(10.0),
            palette.secondary,
        );
    }
    let step = plot.width() / count as f32;
    let bar_width = (step * 0.62).clamp(2.0, 28.0);
    for (index, (date, ms, plays)) in values.iter().enumerate() {
        let center = plot.left() + (index as f32 + 0.5) * step;
        let height = plot.height() * value(*ms, *plays) as f32 / ceiling as f32;
        let hit = Rect::from_min_max(
            pos2(center - step / 2.0, plot.top()),
            pos2(center + step / 2.0, plot.bottom()),
        );
        let response = ui.interact(hit, ui.id().with(("stats-day", index)), Sense::hover());
        let date_label = util::format_date(locale, &date.to_string());
        let play_label = ngettext(
            locale,
            "{count} counted play",
            "{count} counted plays",
            *plays as u32,
        )
        .replace("{count}", &plays.to_string());
        let label = format!(
            "{date_label} · {} · {play_label}",
            util::format_total_ms(locale, *ms)
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
        if response.hovered() {
            ui.painter().rect_filled(hit, 4, palette.surface_hover);
        }
        if height > 0.0 {
            let bar = Rect::from_min_max(
                pos2(center - bar_width / 2.0, plot.bottom() - height.max(2.0)),
                pos2(center + bar_width / 2.0, plot.bottom()),
            );
            ui.painter().rect_filled(
                bar,
                3,
                if response.hovered() {
                    palette.accent_hover
                } else {
                    palette.accent
                },
            );
        } else if *date <= today {
            ui.painter()
                .circle_filled(pos2(center, plot.bottom() - 2.0), 1.5, palette.dim);
        }
        response.on_hover_ui(|ui| {
            theme::text(ui, &date_label, theme::bold(13.0), palette.text);
            ui.label(util::format_total_ms(locale, *ms));
            ui.label(play_label);
        });
        if index == 0 || index == count - 1 || date.day() % 7 == 0 {
            ui.painter().text(
                pos2(center, plot.bottom() + 10.0),
                egui::Align2::CENTER_TOP,
                date.day().to_string(),
                theme::regular(10.0),
                palette.secondary,
            );
        }
        if *date == today {
            ui.painter()
                .circle_filled(pos2(center, rect.bottom() - 1.0), 2.0, palette.accent);
        }
    }
}

fn insights(ui: &mut egui::Ui, palette: &Palette, locale: Locale, report: &Report) {
    let active: Vec<_> = report
        .days
        .iter()
        .filter(|day| day.listened_ms > 0)
        .collect();
    let best = active.iter().max_by_key(|day| day.listened_ms);
    let mut longest = 0;
    let mut streak = 0;
    let mut previous: Option<Date> = None;
    for day in &active {
        streak = if previous.and_then(|date| date.tomorrow().ok()) == Some(day.date) {
            streak + 1
        } else {
            1
        };
        longest = longest.max(streak);
        previous = Some(day.date);
    }
    let rows = [
        (gettext(locale, "Active days"), active.len().to_string()),
        (
            gettext(locale, "Average / active day"),
            util::format_total_ms(
                locale,
                report
                    .listened_ms
                    .checked_div(active.len() as u64)
                    .unwrap_or(0),
            ),
        ),
        (
            gettext(locale, "Best day"),
            best.map(|day| util::format_date(locale, &day.date.to_string()))
                .unwrap_or_else(|| "—".into()),
        ),
        (
            gettext(locale, "Longest streak"),
            ngettext(locale, "{count} day", "{count} days", longest)
                .replace("{count}", &longest.to_string()),
        ),
    ];
    Frame::new()
        .fill(palette.surface)
        .corner_radius(14)
        .inner_margin(18)
        .show(ui, |ui| {
            ui.set_min_width((ui.available_width() - 1.0).max(0.0));
            theme::text(
                ui,
                gettext(locale, "In this period"),
                theme::bold(16.0),
                palette.text,
            );
            ui.add_space(6.0);
            for (index, (label, value)) in rows.iter().enumerate() {
                ui.horizontal(|ui| {
                    theme::text(ui, label.as_ref(), theme::regular(12.0), palette.secondary);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        theme::text(ui, value, theme::medium(12.0), palette.text);
                    });
                });
                if index < rows.len() - 1 {
                    ui.add_space(2.0);
                }
            }
            ui.add_space(6.0);
            if let Some(best) = best {
                theme::text(
                    ui,
                    util::format_total_ms(locale, best.listened_ms),
                    theme::bold(14.0),
                    palette.accent,
                );
                theme::text(
                    ui,
                    gettext(locale, "on your biggest listening day"),
                    theme::regular(11.0),
                    palette.secondary,
                );
            }
        });
}

fn rankings(app: &App, ui: &mut egui::Ui, report: &Report) -> Option<Action> {
    let palette = &app.palette;
    let locale = app.locale;
    let id = ui.make_persistent_id("stats-ranking");
    let mut ranking = ui
        .data(|data| data.get_temp::<Ranking>(id))
        .unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        theme::text(
            ui,
            gettext(locale, "On repeat"),
            theme::bold(23.0),
            palette.text,
        );
        ui.add_space(12.0);
        for (value, label) in [
            (Ranking::Tracks, gettext(locale, "Tracks")),
            (Ranking::Albums, gettext(locale, "Albums")),
            (Ranking::Artists, gettext(locale, "Artists")),
        ] {
            ui.selectable_value(&mut ranking, value, label);
        }
    });
    ui.data_mut(|data| data.insert_temp(id, ranking));
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(gettext(
            locale,
            "Top 50 · ranked by counted plays, then listening time",
        ))
        .size(12.0)
        .color(palette.secondary),
    );
    ui.add_space(10.0);
    let rows = match ranking {
        Ranking::Tracks => &report.tracks,
        Ranking::Albums => &report.albums,
        Ranking::Artists => &report.artists,
    };
    if rows.is_empty() {
        theme::text(
            ui,
            gettext(locale, "No matching metadata recorded yet"),
            theme::regular(13.0),
            palette.secondary,
        );
    }
    let mut action = None;
    for (index, row) in rows.iter().enumerate() {
        let image = if ranking == Ranking::Artists {
            row.uri
                .strip_prefix("spotify:artist:")
                .and_then(|id| app.known_artist(id))
                .and_then(|artist| crate::api::models::pick_image(&artist.images, 160))
        } else {
            row.image.as_deref()
        };
        let response = ui
            .push_id(("stats-rank", index), |ui| {
                rank_row(app, ui, index, row, image, ranking, report.listened_ms)
            })
            .inner;
        if response.clicked() && !row.uri.is_empty() {
            action = Some(Action::OpenUri(row.uri.clone()));
        }
    }
    ui.add_space(10.0);
    let note = match ranking {
        Ranking::Tracks => gettext(
            locale,
            "A play counts after 30 seconds or half a shorter track. Short skips still add listening time.",
        ),
        Ranking::Albums => gettext(
            locale,
            "Album plays add up track plays, not complete album listens.",
        ),
        Ranking::Artists => gettext(
            locale,
            "Each credited artist receives the track's listening time. Artist totals are not added together.",
        ),
    };
    ui.label(egui::RichText::new(note).small().color(palette.secondary));
    action
}

fn rank_row(
    app: &App,
    ui: &mut egui::Ui,
    index: usize,
    row: &Rank,
    image: Option<&str>,
    ranking: Ranking,
    total_ms: u64,
) -> egui::Response {
    let palette = &app.palette;
    let locale = app.locale;
    let wide = ui.available_width() >= 620.0;
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), 66.0),
        if row.uri.is_empty() {
            Sense::hover()
        } else {
            Sense::click()
        },
    );
    let label = format!(
        "{}. {} · {} · {}",
        index + 1,
        row.name,
        row.plays,
        util::format_total_ms(locale, row.listened_ms)
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            if row.uri.is_empty() {
                egui::WidgetType::Label
            } else {
                egui::WidgetType::Button
            },
            true,
            &label,
        )
    });
    if response.gained_focus() {
        response.scroll_to_me(None);
    }
    if response.hovered() || response.has_focus() || index == 0 {
        ui.painter().rect_filled(
            rect,
            10,
            if response.hovered() || response.has_focus() {
                palette.surface_hover
            } else {
                palette.surface
            },
        );
    }
    let number = Rect::from_min_size(rect.min + vec2(9.0, 0.0), vec2(25.0, rect.height()));
    ui.painter().text(
        number.center(),
        egui::Align2::CENTER_CENTER,
        format!("{:02}", index + 1),
        theme::medium(12.0),
        if index == 0 {
            palette.accent
        } else {
            palette.dim
        },
    );
    let cover = Rect::from_min_size(rect.min + vec2(44.0, 11.0), vec2(44.0, 44.0));
    widgets::paint_cover(
        ui,
        palette,
        image,
        cover,
        if ranking == Ranking::Artists {
            22.0
        } else {
            6.0
        },
        if ranking == Ranking::Artists {
            Icon::Users
        } else {
            Icon::Disc
        },
        Some(app.backend.art()),
    );
    let metric_width = if wide { 220.0 } else { 128.0 };
    let names = Rect::from_min_max(
        pos2(cover.right() + 12.0, rect.top() + 13.0),
        pos2(
            (rect.right() - metric_width - 12.0).max(cover.right() + 13.0),
            rect.bottom() - 10.0,
        ),
    );
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(names)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            theme::text(ui, &row.name, theme::medium(14.0), palette.text);
            if !row.subtitle.is_empty() {
                theme::text(ui, &row.subtitle, theme::regular(12.0), palette.secondary);
            }
        },
    );
    let right = rect.right() - 14.0;
    let time = util::format_total_ms(locale, row.listened_ms);
    if wide {
        ui.painter().text(
            pos2(right - 130.0, rect.top() + 17.0),
            egui::Align2::RIGHT_TOP,
            row.plays.to_string(),
            theme::medium(13.0),
            palette.text,
        );
        ui.painter().text(
            pos2(right, rect.top() + 17.0),
            egui::Align2::RIGHT_TOP,
            time,
            theme::regular(12.0),
            palette.secondary,
        );
    } else {
        ui.painter().text(
            pos2(right, rect.top() + 17.0),
            egui::Align2::RIGHT_TOP,
            format!("{} · {time}", row.plays),
            theme::regular(11.0),
            palette.secondary,
        );
    }
    let share = row.listened_ms as f64 / total_ms.max(1) as f64;
    let bar = Rect::from_min_size(
        pos2(right - metric_width + 20.0, rect.top() + 42.0),
        vec2(metric_width - 20.0, 3.0),
    );
    ui.painter().rect_filled(bar, 2, palette.outline);
    ui.painter().rect_filled(
        Rect::from_min_size(
            bar.min,
            vec2(bar.width() * share.clamp(0.0, 1.0) as f32, bar.height()),
        ),
        2,
        palette
            .accent
            .gamma_multiply(if index == 0 { 0.9 } else { 0.55 }),
    );
    theme::focus_ring(ui, &response);
    ui.add_space(3.0);
    response.on_hover_text(format!(
        "{}\n{}",
        row.name,
        gettext(locale, "{percent}% of listening time")
            .replace("{percent}", &format!("{:.1}", share * 100.0))
    ))
}
