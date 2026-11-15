use crate::host::Event;
use disk_migrator::{
    image,
    layout::Role,
    migration::{self, Phase, Progress, Verification},
    plan::Plan,
    preferences::{Catalog, LANGUAGES, Preferences},
    updates::{self, Available},
};
use egui::{Color32, RichText, Vec2};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use winit::event_loop::EventLoopProxy;

pub enum AppEvent {
    Planned(Box<std::result::Result<Plan, String>>),
    Progress(Progress),
    Finished(std::result::Result<Verification, String>, bool),
    Update(std::result::Result<Option<Available>, String>),
    DownloadProgress(u64, u64),
    Downloaded(std::result::Result<(), String>),
    Picked(bool, Option<PathBuf>),
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Files,
    Layout,
    Review,
    Result,
}
enum UpdateState {
    Idle,
    Checking,
    Current,
    Available(Available),
    Failed(String),
    Downloading(u64, u64),
}
pub struct App {
    proxy: EventLoopProxy<Event>,
    prefs: Preferences,
    catalog: Catalog,
    prefs_path: Option<PathBuf>,
    source: String,
    destination: String,
    size: String,
    sector: u32,
    step: Step,
    plan: Option<Plan>,
    confirmation: String,
    busy: bool,
    inspecting: bool,
    progress: Option<Progress>,
    cancel: Arc<AtomicBool>,
    report: Option<Verification>,
    error: Option<String>,
    paused: bool,
    settings: bool,
    update: UpdateState,
    closing: bool,
    pub exit_requested: bool,
}
impl App {
    pub fn new(proxy: EventLoopProxy<Event>) -> Self {
        let prefs_path = Preferences::path().ok();
        let loaded = prefs_path
            .as_ref()
            .map(|p| Preferences::load(p))
            .transpose();
        let (prefs, mut error) = match loaded {
            Ok(p) => (p.unwrap_or_default(), None),
            Err(e) => (Preferences::default(), Some(e.to_string())),
        };
        let args: Vec<_> = std::env::args().collect();
        if args.get(1).is_some_and(|s| s == "--update-error") {
            error = args.get(2).cloned();
        }
        if let Some(path) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join(".DiskMigrator-update-error.txt")))
            && let Ok(message) = std::fs::read_to_string(&path)
        {
            error = Some(message);
            let _ = std::fs::remove_file(path);
        }
        let catalog = Catalog::new(&prefs.language);
        Self {
            proxy,
            prefs,
            catalog,
            prefs_path,
            source: String::new(),
            destination: String::new(),
            size: String::new(),
            sector: 512,
            step: Step::Files,
            plan: None,
            confirmation: String::new(),
            busy: false,
            inspecting: false,
            progress: None,
            cancel: Arc::new(AtomicBool::new(false)),
            report: None,
            error,
            paused: false,
            settings: false,
            update: UpdateState::Idle,
            closing: false,
            exit_requested: false,
        }
    }
    pub fn startup(&mut self) {
        if updates::startup_check_enabled(&self.prefs) {
            self.check_updates();
        }
    }
    pub fn busy(&self) -> bool {
        self.busy
    }
    pub fn request_close(&mut self) {
        self.closing = true;
        self.cancel.store(true, Ordering::Relaxed);
    }
    fn check_updates(&mut self) {
        self.update = UpdateState::Checking;
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = updates::check().map_err(|e| e.to_string());
            let _ = proxy.send_event(Event::App(AppEvent::Update(result)));
        });
    }
    fn inspect(&mut self) {
        let source = PathBuf::from(&self.source);
        let destination = PathBuf::from(&self.destination);
        let sector = self.sector;
        let size = self
            .size
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0 && *n <= 16 * 1024 * 1024)
            .map(|n| n * 1024 * 1024);
        if !destination.exists() && !self.size.trim().is_empty() && size.is_none() {
            self.error = Some(self.catalog.text("size_error").into());
            return;
        }
        self.busy = true;
        self.inspecting = true;
        self.error = None;
        self.confirmation.clear();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = (|| -> disk_migrator::Result<Plan> {
                let s = image::inspect(&source, sector)?;
                let bytes = if destination.exists() {
                    image::identity(&image::open_source(&destination)?)?.bytes
                } else {
                    size.unwrap_or(s.identity.bytes)
                };
                Plan::build(s, &destination, bytes)
            })();
            let _ = proxy.send_event(Event::App(AppEvent::Planned(Box::new(
                result.map_err(|e| e.to_string()),
            ))));
        });
    }
    fn start_copy(&mut self) {
        let Some(plan) = self.plan.clone() else {
            return;
        };
        let confirmation = self.confirmation.clone();
        let resume = plan.checkpoint_path().exists();
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        self.busy = true;
        self.error = None;
        self.paused = false;
        self.report = None;
        self.progress = None;
        self.step = Step::Result;
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let mut last = Instant::now() - Duration::from_secs(1);
            let mut phase = None;
            let result = migration::execute(&plan, &confirmation, resume, &cancel, |p| {
                if last.elapsed() >= Duration::from_millis(120)
                    || phase.as_ref() != Some(&p.phase)
                    || p.done == p.total
                {
                    last = Instant::now();
                    phase = Some(p.phase.clone());
                    let _ = proxy.send_event(Event::App(AppEvent::Progress(p)));
                }
            });
            let paused = matches!(result, Err(disk_migrator::Error::Cancelled));
            let _ = proxy.send_event(Event::App(AppEvent::Finished(
                result.map_err(|e| e.to_string()),
                paused,
            )));
        });
    }
    fn pick(&mut self, source: bool) {
        self.busy = true;
        let proxy = self.proxy.clone();
        let filter = self
            .catalog
            .text(if source { "source" } else { "destination" })
            .to_owned();
        std::thread::spawn(move || {
            let dialog = rfd::FileDialog::new().add_filter(filter, &["img", "raw"]);
            let path = if source {
                dialog.pick_file()
            } else {
                dialog.set_file_name("new-ssd.img").save_file()
            };
            let _ = proxy.send_event(Event::App(AppEvent::Picked(source, path)));
        });
    }
    pub fn on_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Planned(result) => {
                self.busy = false;
                self.inspecting = false;
                match *result {
                    Ok(plan) => {
                        self.plan = Some(plan);
                        self.step = Step::Layout;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            AppEvent::Progress(p) => self.progress = Some(p),
            AppEvent::Finished(result, paused) => {
                self.busy = false;
                self.paused = paused;
                match result {
                    Ok(v) => self.report = Some(v),
                    Err(e) => self.error = Some(e),
                }
            }
            AppEvent::Update(result) => {
                self.update = match result {
                    Ok(Some(a)) => UpdateState::Available(a),
                    Ok(None) => UpdateState::Current,
                    Err(e) => UpdateState::Failed(e),
                }
            }
            AppEvent::DownloadProgress(done, total) => {
                self.update = UpdateState::Downloading(done, total)
            }
            AppEvent::Downloaded(result) => {
                self.busy = false;
                match result {
                    Ok(()) => self.exit_requested = true,
                    Err(e) => self.update = UpdateState::Failed(e),
                }
            }
            AppEvent::Picked(source, Some(path)) => {
                self.busy = false;
                if source {
                    self.source = path.to_string_lossy().into()
                } else {
                    self.destination = path.to_string_lossy().into()
                }
            }
            AppEvent::Picked(_, None) => self.busy = false,
        }
    }
    pub fn ui(&mut self, ctx: &egui::Context) {
        self.style(ctx);
        let c = self.catalog.clone();
        let accent = if self.prefs.dark {
            Color32::from_rgb(106, 218, 190)
        } else {
            Color32::from_rgb(0, 106, 93)
        };
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(32))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(c.text("tag"))
                                .size(12.0)
                                .strong()
                                .color(accent),
                        );
                        ui.label(RichText::new("Disk Migrator").size(28.0).strong());
                        ui.label(c.text("subtitle"));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(c.text("settings")).clicked() {
                            self.settings = true;
                        }
                        if matches!(self.update, UpdateState::Available(_))
                            && ui.button(c.text("update_available")).clicked()
                        {
                            self.settings = true;
                        }
                    });
                });
                ui.add_space(24.0);
                ui.horizontal_wrapped(|ui| {
                    for (i, (step, key)) in [
                        (Step::Files, "step_files"),
                        (Step::Layout, "step_layout"),
                        (Step::Review, "step_review"),
                        (Step::Result, "step_result"),
                    ]
                    .iter()
                    .enumerate()
                    {
                        let selected = self.step == *step;
                        let text = RichText::new(format!("{}  {}", i + 1, c.text(key))).size(15.0);
                        ui.label(if selected {
                            text.color(accent).strong()
                        } else {
                            text.color(ui.visuals().weak_text_color())
                        });
                        if i < 3 {
                            ui.label("   ›   ");
                        }
                    }
                });
                let step_number = match self.step {
                    Step::Files => 0.0,
                    Step::Layout => 1.0,
                    Step::Review => 2.0,
                    Step::Result => 3.0,
                };
                let animated = ctx.animate_value_with_time(
                    egui::Id::new("wizard-transition"),
                    step_number,
                    if self.prefs.reduced_motion { 0.0 } else { 0.18 },
                );
                let (rail, _) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), 3.0),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(
                    rail,
                    1.0,
                    ui.visuals().widgets.noninteractive.bg_stroke.color,
                );
                let filled = egui::Rect::from_min_size(
                    rail.min,
                    Vec2::new(rail.width() * (animated + 1.0) / 4.0, 3.0),
                );
                ui.painter().rect_filled(filled, 1.0, accent);
                ui.add_space(20.0);
                ui.separator();
                ui.add_space(12.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.closing {
                            ui.label(c.text("quit_wait"));
                        }
                        match self.step {
                            Step::Files => self.files_ui(ui, &c),
                            Step::Layout => self.layout_ui(ui, &c),
                            Step::Review => self.review_ui(ui, &c),
                            Step::Result => self.result_ui(ui, &c),
                        }
                        if self.error.is_some() && self.step != Step::Result {
                            self.error_ui(ui, &c);
                        }
                        ui.add_space(24.0);
                        ui.separator();
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(c.text("mode_images"))
                                .size(13.0)
                                .color(ui.visuals().weak_text_color()),
                        );
                    });
            });
        if self.settings {
            self.settings_ui(ctx, &c);
        }
    }
    fn style(&self, ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        let mut v = if self.prefs.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        v.panel_fill = if self.prefs.dark {
            Color32::from_rgb(19, 29, 39)
        } else {
            Color32::from_rgb(245, 248, 250)
        };
        v.window_fill = if self.prefs.dark {
            Color32::from_rgb(25, 38, 49)
        } else {
            Color32::WHITE
        };
        v.selection.bg_fill = if self.prefs.dark {
            Color32::from_rgb(25, 91, 80)
        } else {
            Color32::from_rgb(196, 237, 225)
        };
        v.selection.stroke = egui::Stroke::new(
            1.5,
            if self.prefs.dark {
                Color32::from_rgb(137, 235, 207)
            } else {
                Color32::from_rgb(0, 91, 79)
            },
        );
        v.widgets.inactive.corner_radius = egui::CornerRadius::same(7);
        v.widgets.hovered.corner_radius = egui::CornerRadius::same(7);
        v.widgets.active.corner_radius = egui::CornerRadius::same(7);
        style.visuals = v;
        style.spacing.item_spacing = Vec2::new(12.0, 12.0);
        style.spacing.button_padding = Vec2::new(16.0, 10.0);
        style.spacing.interact_size = Vec2::new(40.0, 40.0);
        style.animation_time = if self.prefs.reduced_motion { 0.0 } else { 0.16 };
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(16.0));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(24.0));
        ctx.set_style(style);
    }
    fn heading(ui: &mut egui::Ui, c: &Catalog, title: &str, help: &str) {
        ui.heading(c.text(title));
        ui.label(c.text(help));
        ui.add_space(16.0);
    }
    fn files_ui(&mut self, ui: &mut egui::Ui, c: &Catalog) {
        Self::heading(ui, c, "files_title", "files_help");
        ui.add_enabled_ui(!self.busy, |ui| {
            let source_label = ui.label(RichText::new(c.text("source")).strong());
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 150.0).max(150.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.source)
                        .hint_text(c.text("source_hint"))
                        .desired_width(width),
                )
                .labelled_by(source_label.id);
                if ui.button(c.text("browse")).clicked() {
                    self.pick(true);
                }
            });
            let destination_label = ui.label(RichText::new(c.text("destination")).strong());
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 150.0).max(150.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.destination)
                        .hint_text(c.text("destination_hint"))
                        .desired_width(width),
                )
                .labelled_by(destination_label.id);
                if ui.button(c.text("browse")).clicked() {
                    self.pick(false);
                }
            });
            ui.add_space(12.0);
            ui.label(c.text("sector"));
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.sector, 512, "512 B");
                ui.selectable_value(&mut self.sector, 4096, "4096 B");
            });
            let size_label = ui.label(c.text("new_size"));
            ui.add(
                egui::TextEdit::singleline(&mut self.size)
                    .desired_width(240.0)
                    .hint_text("32768"),
            )
            .labelled_by(size_label.id);
            ui.label(RichText::new(c.text("size_help")).size(13.0));
            ui.add_space(16.0);
            if primary(
                ui,
                c.text("inspect"),
                !self.source.trim().is_empty() && !self.destination.trim().is_empty(),
            )
            .clicked()
            {
                self.inspect();
            }
        });
        if self.inspecting {
            ui.label(c.text("reviewing"));
        }
        ui.add_space(12.0);
        ui.label(c.text("no_physical"));
    }
    fn layout_ui(&mut self, ui: &mut egui::Ui, c: &Catalog) {
        let Some(p) = self.plan.clone() else { return };
        Self::heading(ui, c, "layout_title", "layout_help");
        egui::Grid::new("capacities")
            .spacing([28.0, 12.0])
            .show(ui, |ui| {
                for (key, bytes) in [
                    ("source_size", p.source.identity.bytes),
                    ("destination_size", p.destination_bytes),
                    ("unallocated", p.destination_bytes - p.source.identity.bytes),
                ] {
                    ui.label(c.text(key));
                    ui.label(RichText::new(capacity(bytes)).strong());
                    ui.end_row();
                }
            });
        ui.add_space(16.0);
        partition_bar(ui, &p);
        for part in &p.source.layout.partitions {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{} {}", c.text("partition"), part.index)).strong());
                ui.label(c.text(role_key(&part.role)));
                ui.label(capacity(part.bytes(p.source.layout.sector_size)));
                if !part.name.is_empty() {
                    ui.label(&part.name);
                }
            });
        }
        if p.source.layout.partitions.is_empty() {
            ui.label(c.text("no_partitions"));
        }
        ui.add_space(12.0);
        ui.label(c.text(if p.is_gpt() {
            "gpt_relocate"
        } else {
            "mbr_preserve"
        }));
        ui.label(c.text("no_resize"));
        egui::CollapsingHeader::new(c.text("details")).show(ui, |ui| {
            ui.monospace(format!(
                "{:?} · {} B · {}",
                p.source.layout.scheme, p.source.layout.sector_size, p.id
            ));
            for part in &p.source.layout.partitions {
                ui.label(format!(
                    "{}: LBA {}–{} · {}",
                    part.index,
                    part.first_lba,
                    part.last_lba,
                    c.text(if part.aligned { "aligned" } else { "unaligned" })
                ));
            }
            ui.label(c.text("support_status"));
        });
        ui.add_space(16.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button(c.text("back")).clicked() {
                self.step = Step::Files;
            }
            if primary(ui, c.text("next"), true).clicked() {
                self.step = Step::Review;
            }
        });
    }
    fn review_ui(&mut self, ui: &mut egui::Ui, c: &Catalog) {
        let Some(p) = self.plan.clone() else { return };
        Self::heading(ui, c, "review_title", "review_help");
        ui.label(RichText::new(c.text("source")).strong());
        ui.label(p.source.path.display().to_string());
        ui.add_space(8.0);
        ui.label(RichText::new(c.text("destination")).strong());
        ui.label(p.destination.display().to_string());
        ui.label(capacity(p.destination_bytes));
        ui.add_space(12.0);
        ui.label(
            RichText::new(c.text(if p.destination.exists() {
                "overwrite"
            } else {
                "create_image"
            }))
            .strong(),
        );
        ui.label(c.text("source_preserved"));
        let resume = p.checkpoint_path().exists();
        if resume {
            ui.label(c.text("checkpoint_available"));
        }
        ui.add_space(16.0);
        let confirmation_label = ui.label(c.text("confirmation"));
        ui.label(RichText::new(p.confirmation_name()).monospace());
        ui.add(egui::TextEdit::singleline(&mut self.confirmation).desired_width(420.0))
            .labelled_by(confirmation_label.id);
        ui.add_space(16.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button(c.text("back")).clicked() {
                self.step = Step::Layout;
            }
            if primary(
                ui,
                c.text(if resume { "resume" } else { "start" }),
                self.confirmation == p.confirmation_name() && !self.busy,
            )
            .clicked()
            {
                self.start_copy();
            }
        });
    }
    fn result_ui(&mut self, ui: &mut egui::Ui, c: &Catalog) {
        if self.busy {
            ui.heading(c.text("running_title"));
            ui.add_space(16.0);
            if let Some(p) = &self.progress {
                let key = match p.phase {
                    Phase::Fingerprinting => "phase_hash",
                    Phase::Resuming => "phase_resume",
                    Phase::Copying => "phase_copy",
                    Phase::Verifying => "phase_verify",
                };
                ui.label(c.text(key));
                let fraction = p.done as f32 / p.total.max(1) as f32;
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .show_percentage()
                        .animate(false)
                        .desired_width(ui.available_width()),
                );
                ui.label(format!("{} / {}", capacity(p.done), capacity(p.total)));
            }
            ui.add_space(16.0);
            if ui
                .add_enabled(
                    !self.cancel.load(Ordering::Relaxed),
                    egui::Button::new(c.text("cancel")),
                )
                .clicked()
            {
                self.cancel.store(true, Ordering::Relaxed);
            }
            ui.label(c.text("pause_detail"));
        } else if let Some(v) = &self.report {
            Self::heading(ui, c, "success_title", "success_help");
            for (key, passed) in [
                ("verification_data", true),
                ("verification_table", true),
                ("verification_fs", false),
                ("verification_boot", false),
            ] {
                ui.horizontal_wrapped(|ui| {
                    ui.label(if passed { "✓" } else { "—" });
                    ui.label(c.text(key));
                    ui.label(
                        RichText::new(c.text(if passed { "passed" } else { "not_tested" }))
                            .strong(),
                    );
                });
            }
            egui::CollapsingHeader::new(c.text("details")).show(ui, |ui| {
                ui.label(c.text("source_hash"));
                ui.monospace(&v.source_sha256);
                ui.label(format!(
                    "{}: {}",
                    c.text("verified_bytes"),
                    v.verified_bytes
                ));
            });
            ui.add_space(16.0);
            if primary(ui, c.text("new_migration"), true).clicked() {
                self.step = Step::Files;
                self.plan = None;
                self.report = None;
                self.error = None;
                self.confirmation.clear();
            }
        } else {
            ui.heading(c.text(if self.paused { "paused" } else { "failed" }));
            self.error_ui(ui, c);
            ui.add_space(16.0);
            if ui.button(c.text("retry")).clicked() {
                self.error = None;
                self.step = Step::Review;
            }
        }
    }
    fn error_ui(&self, ui: &mut egui::Ui, c: &Catalog) {
        if let Some(e) = &self.error {
            ui.add_space(12.0);
            if !self.paused {
                ui.colored_label(ui.visuals().error_fg_color, c.text("error_generic"));
            }
            egui::CollapsingHeader::new(c.text("error_details")).show(ui, |ui| {
                ui.label(e);
            });
        }
    }
    fn settings_ui(&mut self, ctx: &egui::Context, c: &Catalog) {
        let before = self.prefs.clone();
        let mut open = true;
        egui::Window::new(c.text("settings"))
            .id(egui::Id::new("settings"))
            .open(&mut open)
            .resizable(true)
            .default_width(520.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(540.0)
                    .show(ui, |ui| {
                        ui.label(c.text("language"));
                        egui::ComboBox::from_id_salt("language")
                            .selected_text(
                                LANGUAGES
                                    .iter()
                                    .find(|(code, _)| *code == self.prefs.language)
                                    .map(|(_, name)| *name)
                                    .unwrap_or("English"),
                            )
                            .show_ui(ui, |ui| {
                                for (code, name) in LANGUAGES {
                                    ui.selectable_value(
                                        &mut self.prefs.language,
                                        code.to_owned(),
                                        name,
                                    );
                                }
                            });
                        ui.label(c.text("appearance"));
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut self.prefs.dark, false, c.text("light"));
                            ui.selectable_value(&mut self.prefs.dark, true, c.text("dark"));
                        });
                        ui.checkbox(&mut self.prefs.reduced_motion, c.text("reduced_motion"));
                        ui.separator();
                        ui.label(format!(
                            "{} {}",
                            c.text("version"),
                            env!("CARGO_PKG_VERSION")
                        ));
                        ui.checkbox(&mut self.prefs.startup_updates, c.text("startup_updates"));
                        if ui
                            .add_enabled(
                                !matches!(
                                    self.update,
                                    UpdateState::Checking | UpdateState::Downloading(..)
                                ),
                                egui::Button::new(c.text("check_updates")),
                            )
                            .clicked()
                        {
                            self.check_updates();
                        }
                        match &self.update {
                            UpdateState::Idle => {}
                            UpdateState::Checking => {
                                ui.label(c.text("checking"));
                            }
                            UpdateState::Current => {
                                ui.label(c.text("up_to_date"));
                            }
                            UpdateState::Failed(e) => {
                                ui.label(c.text("update_failed"));
                                egui::CollapsingHeader::new(c.text("error_details")).show(
                                    ui,
                                    |ui| {
                                        ui.label(e);
                                    },
                                );
                            }
                            UpdateState::Available(update) => {
                                ui.label(format!(
                                    "{} · {}",
                                    c.text("update_available"),
                                    update.version
                                ));
                                let update = update.clone();
                                if ui
                                    .add_enabled(
                                        !self.busy,
                                        egui::Button::new(c.text("download_install")),
                                    )
                                    .clicked()
                                {
                                    match std::env::current_exe()
                                        .ok()
                                        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                                    {
                                        Some(parent) => {
                                            self.busy = true;
                                            self.update = UpdateState::Downloading(0, update.bytes);
                                            let proxy = self.proxy.clone();
                                            std::thread::spawn(move || {
                                                let mut last =
                                                    Instant::now() - Duration::from_secs(1);
                                                let result = updates::download(
                                                    &update,
                                                    &parent,
                                                    |done, total| {
                                                        if last.elapsed()
                                                            > Duration::from_millis(150)
                                                            || done == total
                                                        {
                                                            last = Instant::now();
                                                            let _ = proxy.send_event(Event::App(
                                                                AppEvent::DownloadProgress(
                                                                    done, total,
                                                                ),
                                                            ));
                                                        }
                                                    },
                                                )
                                                .and_then(|staged| {
                                                    updates::launch_installer(&staged, &update)
                                                });
                                                let _ = proxy.send_event(Event::App(
                                                    AppEvent::Downloaded(
                                                        result.map_err(|e| e.to_string()),
                                                    ),
                                                ));
                                            });
                                        }
                                        None => {
                                            self.update = UpdateState::Failed(
                                                "Executable directory unavailable.".into(),
                                            )
                                        }
                                    }
                                }
                                ui.label(c.text("restart_help"));
                            }
                            UpdateState::Downloading(done, total) => {
                                ui.label(c.text("downloading"));
                                ui.add(
                                    egui::ProgressBar::new(*done as f32 / (*total).max(1) as f32)
                                        .show_percentage()
                                        .animate(false),
                                );
                            }
                        }
                        ui.separator();
                        egui::CollapsingHeader::new(c.text("licenses")).show(ui, |ui| {
                            static LINES: std::sync::OnceLock<Vec<&'static str>> =
                                std::sync::OnceLock::new();
                            let lines = LINES.get_or_init(|| {
                                include_str!("../LICENSE")
                                    .lines()
                                    .chain(
                                        include_str!("../assets/THIRD-PARTY-NOTICES.txt").lines(),
                                    )
                                    .collect()
                            });
                            egui::ScrollArea::both()
                                .id_salt("licenses-scroll")
                                .max_height(200.0)
                                .show_rows(ui, 18.0, lines.len(), |ui, rows| {
                                    for i in rows {
                                        ui.label(RichText::new(lines[i]).size(12.0).monospace());
                                    }
                                });
                        });
                        ui.label(c.text("saved"));
                        if ui.button(c.text("close")).clicked() {
                            self.settings = false;
                        }
                    });
            });
        if !open {
            self.settings = false;
        }
        if before != self.prefs {
            self.catalog = Catalog::new(&self.prefs.language);
            if let Some(path) = &self.prefs_path
                && let Err(e) = self.prefs.save(path)
            {
                self.error = Some(e.to_string());
            }
        }
    }
}
fn primary(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let fill = if ui.visuals().dark_mode {
        Color32::from_rgb(106, 218, 190)
    } else {
        Color32::from_rgb(0, 106, 93)
    };
    let ink = if ui.visuals().dark_mode {
        Color32::from_rgb(15, 42, 37)
    } else {
        Color32::WHITE
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).strong().color(ink))
            .fill(fill)
            .min_size(Vec2::new(150.0, 44.0)),
    )
}
fn capacity(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}
fn role_key(role: &Role) -> &'static str {
    match role {
        Role::Efi => "role_efi",
        Role::Reserved => "role_reserved",
        Role::Data => "role_data",
        Role::Recovery => "role_recovery",
        Role::System => "role_system",
        Role::Other => "role_other",
    }
}
fn partition_bar(ui: &mut egui::Ui, p: &Plan) {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 5.0, ui.visuals().extreme_bg_color);
    for part in &p.source.layout.partitions {
        let start =
            part.first_lba as f64 * p.source.layout.sector_size as f64 / p.destination_bytes as f64;
        let width = part.bytes(p.source.layout.sector_size) as f64 / p.destination_bytes as f64;
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left() + rect.width() * start as f32, rect.top()),
            Vec2::new((rect.width() * width as f32).max(2.0), rect.height()),
        );
        ui.painter().rect_filled(
            bar,
            3.0,
            if ui.visuals().dark_mode {
                Color32::from_rgb(89, 191, 169)
            } else {
                Color32::from_rgb(0, 119, 106)
            },
        );
    }
}
