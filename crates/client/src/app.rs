use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, Receiver};
use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2,
};
use omarchy_protocol::{keys, InputEvent};
use tokio::sync::mpsc::unbounded_channel;

use crate::keys::evdev_code;
use crate::net::{
    parse_host, parse_pin, run_session, ClientCommand, SessionConfig, UiEvent, UiSink, VideoFrame,
};

pub struct GuiLaunch {
    /// Connect immediately when the command line already has a host and pin.
    pub preset: Option<SessionConfig>,
    pub host_text: String,
    pub pin_text: String,
    pub download_dir: PathBuf,
    pub send_file: Option<PathBuf>,
}

pub fn run_gui(launch: GuiLaunch) -> anyhow::Result<()> {
    let session = launch.preset.map(spawn_session);
    let app = Shell {
        host_text: if launch.host_text.is_empty() {
            load_saved_host()
        } else {
            launch.host_text
        },
        pin_text: launch.pin_text,
        form_error: String::new(),
        download_dir: launch.download_dir,
        send_file: launch.send_file,
        focused: false,
        session,
    };
    let mut options = eframe::NativeOptions::default();
    let size = if app.session.is_some() {
        [1180.0, 720.0]
    } else {
        [520.0, 420.0]
    };
    options.viewport = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_min_inner_size([420.0, 360.0])
        .with_title("Omarchy Connect");
    eframe::run_native(
        "Omarchy Connect",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(visuals());
            Ok(Box::new(app))
        }),
    )
    .map_err(|err| anyhow::anyhow!(err.to_string()))
}

fn spawn_session(config: SessionConfig) -> ClientApp {
    let (frame_tx, frame_rx) = sync_channel(2);
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let sink = UiSink {
        frames: frame_tx,
        events: event_tx.clone(),
    };
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(err) => {
                let _ = event_tx.send(UiEvent::Closed(err.to_string()));
                return;
            }
        };
        if let Err(err) = runtime.block_on(run_session(config, cmd_rx, Some(sink))) {
            let _ = event_tx.send(UiEvent::Closed(err.to_string()));
        }
    });
    ClientApp {
        frames: frame_rx,
        events: event_rx,
        commands: cmd_tx,
        status: "Starting".into(),
        displays: Vec::new(),
        textures: HashMap::new(),
        hits: Vec::new(),
        send_path: String::new(),
        file_status: "No file transfer yet".into(),
        hovered: None,
        shift: false,
        ctrl: false,
        alt: false,
        frame_count: 0,
        leave: false,
    }
}

struct Shell {
    host_text: String,
    pin_text: String,
    form_error: String,
    download_dir: PathBuf,
    send_file: Option<PathBuf>,
    focused: bool,
    session: Option<ClientApp>,
}

impl eframe::App for Shell {
    fn on_exit(&mut self) {
        if let Some(session) = &self.session {
            let _ = session.commands.send(ClientCommand::Disconnect);
        }
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.session.as_ref().is_some_and(|session| session.leave) {
            if let Some(session) = self.session.take() {
                let _ = session.commands.send(ClientCommand::Disconnect);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(520.0, 420.0)));
        }
        if self.session.is_some() {
            eframe::App::update(self.session.as_mut().unwrap(), ctx, frame);
            return;
        }
        self.connect_form(ctx);
    }
}

impl Shell {
    fn connect_form(&mut self, ctx: &egui::Context) {
        let amber = Color32::from_rgb(232, 168, 56);
        let mut connect = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(36.0);
            ui.vertical_centered(|ui| {
                ui.colored_label(
                    amber,
                    egui::RichText::new("Omarchy Connect").size(28.0),
                );
                ui.add_space(6.0);
                ui.label("Type the IP address printed on your Omarchy machine.");
            });
            ui.add_space(28.0);
            let width = 360.0;
            let left = ((ui.available_width() - width) * 0.5).max(0.0);
            ui.horizontal(|ui| {
                ui.add_space(left);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    ui.label("Omarchy IP");
                    let host = ui.add(
                        egui::TextEdit::singleline(&mut self.host_text)
                            .hint_text("192.168.1.10")
                            .desired_width(width)
                            .font(FontId::proportional(18.0)),
                    );
                    if !self.focused {
                        host.request_focus();
                        self.focused = true;
                    }
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Port 47921 is used when you leave it off.")
                            .size(12.0)
                            .color(Color32::from_rgb(160, 152, 140)),
                    );
                    ui.add_space(14.0);
                    ui.label("Pin");
                    let pin = ui.add(
                        egui::TextEdit::singleline(&mut self.pin_text)
                            .hint_text("SHA-256 pin from the host")
                            .desired_width(width)
                            .font(FontId::monospace(14.0)),
                    );
                    ui.add_space(18.0);
                    let button = ui.add_sized(
                        [width, 36.0],
                        egui::Button::new(egui::RichText::new("Connect").size(16.0)),
                    );
                    connect = button.clicked()
                        || (host.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        || (pin.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if !self.form_error.is_empty() {
                        ui.add_space(12.0);
                        ui.colored_label(
                            Color32::from_rgb(220, 96, 72),
                            egui::RichText::new(&self.form_error).size(14.0),
                        );
                    }
                });
            });
        });
        if connect {
            self.try_connect(ctx);
        }
    }

    fn try_connect(&mut self, ctx: &egui::Context) {
        let addr = match parse_host(&self.host_text) {
            Ok(addr) => addr,
            Err(err) => {
                self.form_error = err.to_string();
                return;
            }
        };
        let pin = match parse_pin(&self.pin_text) {
            Ok(pin) => pin,
            Err(err) => {
                self.form_error = err.to_string();
                return;
            }
        };
        self.form_error.clear();
        save_host(&self.host_text);
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(1180.0, 720.0)));
        self.session = Some(spawn_session(SessionConfig {
            addr,
            pin,
            download_dir: self.download_dir.clone(),
            send_file: self.send_file.clone(),
            stop_after_frames: None,
        }));
    }
}

fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("omarchy-connect").join("host.txt"))
}

fn load_saved_host() -> String {
    let Some(path) = settings_path() else {
        return String::new();
    };
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn save_host(host: &str) {
    let Some(path) = settings_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, host.trim());
}

struct ClientApp {
    frames: Receiver<VideoFrame>,
    events: Receiver<UiEvent>,
    commands: tokio::sync::mpsc::UnboundedSender<ClientCommand>,
    status: String,
    displays: Vec<omarchy_protocol::DisplayInfo>,
    textures: HashMap<u32, egui::TextureHandle>,
    hits: Vec<(omarchy_protocol::DisplayInfo, Rect)>,
    send_path: String,
    file_status: String,
    hovered: Option<u32>,
    shift: bool,
    ctrl: bool,
    alt: bool,
        frame_count: u32,
        leave: bool,
}

impl eframe::App for ClientApp {
    fn on_exit(&mut self) {
        let _ = self.commands.send(ClientCommand::Disconnect);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain(ctx);
        let amber = Color32::from_rgb(232, 168, 56);
        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.colored_label(amber, "Omarchy Connect");
                ui.label(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Change host").clicked() {
                        self.leave = true;
                    }
                    ui.label(format!("{} frames", self.frame_count));
                });
            });
            ui.add_space(6.0);
        });
        let mut path_id = None;
        egui::TopBottomPanel::bottom("files").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Send");
                let editor = ui.add(
                    egui::TextEdit::singleline(&mut self.send_path)
                        .hint_text("Path on this machine")
                        .desired_width(320.0),
                );
                path_id = Some(editor.id);
                if ui.button("Send file").clicked() {
                    let path = PathBuf::from(self.send_path.trim());
                    if !path.as_os_str().is_empty() {
                        ctx.memory_mut(|mem| mem.surrender_focus(editor.id));
                        let _ = self.commands.send(ClientCommand::SendFile(path));
                    }
                }
                ui.label(&self.file_status);
            });
            ui.add_space(6.0);
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            self.paint_displays(ui);
        });
        if let Some(path_id) = path_id {
            self.apply_input(ctx, path_id);
        }
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

impl ClientApp {
    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(frame) = self.frames.try_recv() {
            self.frame_count = self.frame_count.saturating_add(1);
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.rgba,
            );
            if let Some(texture) = self.textures.get_mut(&frame.display_id) {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                let texture = ctx.load_texture(
                    format!("display-{}", frame.display_id),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                self.textures.insert(frame.display_id, texture);
            }
        }
        while let Ok(event) = self.events.try_recv() {
            match event {
                UiEvent::Status(text) => self.status = text,
                UiEvent::Displays(displays) => self.displays = displays,
                UiEvent::File {
                    incoming,
                    name,
                    transferred,
                    total,
                    done,
                } => {
                    let direction = if incoming { "Receiving" } else { "Sending" };
                    let state = if done { "done" } else { "in progress" };
                    self.file_status =
                        format!("{direction} {name}: {transferred}/{total} bytes ({state})");
                }
                UiEvent::Closed(text) => self.status = text,
            }
        }
    }

    fn paint_displays(&mut self, ui: &mut egui::Ui) {
        if self.displays.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("Waiting for the host to describe its displays.");
            });
            self.hits.clear();
            self.hovered = None;
            return;
        }
        let canvas = ui.available_rect_before_wrap();
        let min_x = self.displays.iter().map(|d| d.x).min().unwrap_or(0);
        let min_y = self.displays.iter().map(|d| d.y).min().unwrap_or(0);
        let max_x = self
            .displays
            .iter()
            .map(|d| d.x.saturating_add(d.width as i32))
            .max()
            .unwrap_or(1);
        let max_y = self
            .displays
            .iter()
            .map(|d| d.y.saturating_add(d.height as i32))
            .max()
            .unwrap_or(1);
        let desk_w = (max_x - min_x).max(1) as f32;
        let desk_h = (max_y - min_y).max(1) as f32;
        let scale = (canvas.width() / desk_w)
            .min(canvas.height() / desk_h)
            .max(0.01);
        let origin = canvas.min
            + Vec2::new(
                (canvas.width() - desk_w * scale) * 0.5,
                (canvas.height() - desk_h * scale) * 0.5,
            );
        self.hits.clear();
        self.hovered = None;
        let amber = Color32::from_rgb(232, 168, 56);
        for display in &self.displays {
            let x = origin.x + (display.x - min_x) as f32 * scale;
            let y = origin.y + (display.y - min_y) as f32 * scale;
            let rect = Rect::from_min_size(
                Pos2::new(x, y),
                Vec2::new(display.width as f32 * scale, display.height as f32 * scale),
            );
            let response = ui.interact(
                rect,
                ui.id().with(("monitor", display.id)),
                Sense::click_and_drag(),
            );
            if response.hovered() {
                self.hovered = Some(display.id);
                response.on_hover_cursor(egui::CursorIcon::Crosshair);
            }
            if let Some(texture) = self.textures.get(&display.id) {
                ui.painter().image(
                    texture.id(),
                    rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            } else {
                ui.painter()
                    .rect_filled(rect, CornerRadius::ZERO, Color32::from_rgb(12, 11, 10));
            }
            ui.painter().rect_stroke(
                rect,
                CornerRadius::ZERO,
                Stroke::new(1.0_f32, amber),
                StrokeKind::Inside,
            );
            let label = format!(
                "{}    {}×{}    {}%",
                display.name, display.width, display.height, display.scale_percent
            );
            ui.painter().text(
                rect.left_top() + Vec2::new(8.0, 6.0),
                Align2::LEFT_TOP,
                label,
                FontId::proportional(13.0),
                Color32::from_rgb(236, 230, 218),
            );
            self.hits.push((display.clone(), rect));
        }
    }

    fn apply_input(&mut self, ctx: &egui::Context, path_id: egui::Id) {
        let typing = ctx.memory(|mem| mem.has_focus(path_id));
        let pointer = ctx.input(|input| input.pointer.hover_pos());
        if !typing {
            if let Some(pos) = pointer {
                if let Some((display, rect)) = self.hit(pos) {
                    let (x, y) = pixel(&display, rect, pos);
                    let _ = self
                        .commands
                        .send(ClientCommand::Input(InputEvent::MouseMove {
                            display_id: display.id,
                            x,
                            y,
                        }));
                }
            }
            let mods = ctx.input(|input| input.modifiers);
            self.sync_mod(keys::KEY_LEFTSHIFT, self.shift, mods.shift);
            self.sync_mod(keys::KEY_LEFTCTRL, self.ctrl, mods.ctrl);
            self.sync_mod(keys::KEY_LEFTALT, self.alt, mods.alt);
            self.shift = mods.shift;
            self.ctrl = mods.ctrl;
            self.alt = mods.alt;
            let events = ctx.input(|input| input.events.clone());
            for event in events {
                match event {
                    egui::Event::Key {
                        key,
                        pressed,
                        repeat,
                        ..
                    } => {
                        if repeat {
                            continue;
                        }
                        if let Some(code) = evdev_code(key) {
                            let _ = self
                                .commands
                                .send(ClientCommand::Input(InputEvent::Key { code, pressed }));
                        }
                    }
                    egui::Event::PointerButton {
                        pos,
                        button,
                        pressed,
                        ..
                    } => {
                        if let Some((display, rect)) = self.hit(pos) {
                            if pressed {
                                ctx.memory_mut(|mem| mem.surrender_focus(path_id));
                            }
                            let (x, y) = pixel(&display, rect, pos);
                            let button = match button {
                                egui::PointerButton::Primary => 0,
                                egui::PointerButton::Secondary => 1,
                                egui::PointerButton::Middle => 2,
                                egui::PointerButton::Extra1 | egui::PointerButton::Extra2 => {
                                    continue
                                }
                            };
                            let _ =
                                self.commands
                                    .send(ClientCommand::Input(InputEvent::MouseMove {
                                        display_id: display.id,
                                        x,
                                        y,
                                    }));
                            let _ =
                                self.commands
                                    .send(ClientCommand::Input(InputEvent::MouseButton {
                                        display_id: display.id,
                                        button,
                                        pressed,
                                    }));
                        }
                    }
                    egui::Event::MouseWheel { unit, delta, .. } => {
                        let Some(display_id) = self.hovered else {
                            continue;
                        };
                        let scale = match unit {
                            egui::MouseWheelUnit::Line => 1.0,
                            egui::MouseWheelUnit::Point => 1.0 / 16.0,
                            egui::MouseWheelUnit::Page => 3.0,
                        };
                        let dx = (delta.x * scale).round() as i32;
                        let dy = (-delta.y * scale).round() as i32;
                        if dx != 0 || dy != 0 {
                            let _ =
                                self.commands
                                    .send(ClientCommand::Input(InputEvent::MouseWheel {
                                        display_id,
                                        dx,
                                        dy,
                                    }));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn sync_mod(&self, code: u16, previous: bool, now: bool) {
        if previous != now {
            let _ = self
                .commands
                .send(ClientCommand::Input(InputEvent::Key { code, pressed: now }));
        }
    }

    fn hit(&self, pos: Pos2) -> Option<(omarchy_protocol::DisplayInfo, Rect)> {
        self.hits
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .cloned()
    }
}

fn pixel(display: &omarchy_protocol::DisplayInfo, rect: Rect, pos: Pos2) -> (u32, u32) {
    let nx = ((pos.x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 0.999);
    let ny = ((pos.y - rect.min.y) / rect.height().max(1.0)).clamp(0.0, 0.999);
    let x = (nx * display.width as f32) as u32;
    let y = (ny * display.height as f32) as u32;
    (
        x.min(display.width.saturating_sub(1)),
        y.min(display.height.saturating_sub(1)),
    )
}

fn visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    let ink = Color32::from_rgb(236, 230, 218);
    let amber = Color32::from_rgb(232, 168, 56);
    visuals.panel_fill = Color32::from_rgb(22, 21, 18);
    visuals.window_fill = Color32::from_rgb(32, 30, 26);
    visuals.extreme_bg_color = Color32::from_rgb(16, 15, 13);
    visuals.faint_bg_color = Color32::from_rgb(38, 35, 30);
    visuals.widgets.noninteractive.fg_stroke.color = ink;
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(48, 43, 36);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(72, 58, 32);
    visuals.widgets.active.bg_fill = amber;
    visuals.widgets.active.fg_stroke.color = Color32::from_rgb(22, 21, 18);
    visuals.selection.bg_fill = amber;
    visuals.selection.stroke.color = Color32::from_rgb(22, 21, 18);
    visuals.hyperlink_color = amber;
    visuals
}
