use super::*;

use crate::video::{PREVIEW_HEIGHT, PREVIEW_WIDTH};

pub(super) fn draw_bik(ui: &mut egui::Ui, state: &mut AppState, doc_id: DocumentId) {
    let Some((bytes, header)) = state.document(doc_id).and_then(|d| match &d.payload {
        DocPayload::Bik(bik) => Some((std::sync::Arc::clone(&bik.bytes), bik.header.clone())),
        _ => None,
    }) else {
        return;
    };
    {
        let Some(mut preview) = state.video_preview(doc_id) else {
            return;
        };
        preview.tick(f64::from(ui.input(|i| i.stable_dt)));
        draw_preview(ui, &mut preview, &bytes, header.as_ref());
    }
    // The speaker is the app's, so what the preview owes it is answered once its lock is let go.
    state.sound_the_movie(doc_id);
}

fn draw_preview(
    ui: &mut egui::Ui,
    preview: &mut crate::video::Preview,
    bytes: &std::sync::Arc<[u8]>,
    header: Option<&crate::bink::Header>,
) {
    ui.heading("Bink Video");
    ui.separator();
    if let Some(header) = header {
        grid(ui, "bik_header", |ui| {
            let kind = if header.is_bink2() {
                "Bink 2"
            } else {
                "Bink 1"
            };
            row(ui, "Format", &format!("{kind} ({})", header.revision()));
            row(ui, "Size", &format!("{} × {}", header.width, header.height));
            row(ui, "Frames", &header.frames.to_string());
            let rate = header
                .fps()
                .map_or_else(|| "not stated".to_string(), |fps| format!("{fps:.3} fps"));
            row(ui, "Frame rate", &rate);
            row(ui, "Audio tracks", &header.audio_tracks.to_string());
        });
    }
    if let Some(problem) = preview.problem() {
        ui.colored_label(theme::WARNING, problem);
    }
    if let Some(texture) = preview.texture(ui.ctx()) {
        let width = ui.available_width();
        let height = width * PREVIEW_HEIGHT as f32 / PREVIEW_WIDTH as f32;
        ui.image((texture.id(), egui::vec2(width, height)));
    }

    ui.horizontal(|ui| {
        if preview.playing {
            if ui.button(format!("{} Pause", ph::PAUSE)).clicked() {
                preview.pause();
            }
        } else if ui.button(format!("{} Play", ph::PLAY)).clicked() {
            preview.play(bytes);
        }
        let mut at = preview.position;
        let end = preview.duration.unwrap_or(at).max(at);
        let slider = ui.add_enabled(
            preview.duration.is_some(),
            egui::Slider::new(&mut at, 0.0..=end).suffix(" s"),
        );
        if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
            preview.seek(bytes, at);
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.label(tr!("Speed"));
        let mut speed = preview.speed();
        egui::ComboBox::from_id_salt("bik_speed")
            .selected_text(format!("×{speed}"))
            .width(72.0)
            .show_ui(ui, |ui| {
                for value in [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 4.0] {
                    ui.selectable_value(&mut speed, value, format!("×{value}"));
                }
            })
            .response
            .on_hover_text(tr!("Playback speed multiplier"));
        preview.set_speed(speed);
        if let Some(progress) = preview.buffer_progress() {
            ui.add(
                egui::ProgressBar::new(progress)
                    .desired_width(180.0)
                    .text(tr!(
                        "Preparing video… {percent}%",
                        percent = format!("{:.0}", progress * 100.0)
                    )),
            );
        }
    });
    ui.label(
        egui::RichText::new(preview.sound_note())
            .small()
            .color(theme::TEXT_SECONDARY),
    );
    if let Some(why) = preview.sound_problem() {
        ui.colored_label(theme::WARNING, why);
    }

    if preview.busy() {
        ui.ctx().request_repaint();
    }
}
