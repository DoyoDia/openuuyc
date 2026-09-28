use crate::media::audio::encoder::Quality;

fn label(kbps: u32) -> &'static str {
    match kbps {
        64 => "省带宽 · 64 kbps",
        128 => "标准 · 128 kbps",
        192 => "高音质 · 192 kbps",
        256 => "极高音质 · 256 kbps",
        _ => "请选择音质档位",
    }
}

pub(crate) fn audio_quality(ui: &mut egui::Ui, quality: &mut Quality) -> bool {
    let previous = *quality;
    egui::ComboBox::from_id_salt("audio-quality")
        .width(238.0)
        .selected_text(label(quality.kbps))
        .show_ui(ui, |ui| {
            for kbps in Quality::RATES {
                ui.selectable_value(&mut quality.kbps, kbps, label(kbps));
            }
        });
    *quality != previous
}

pub(crate) fn audio_quality_segments(ui: &mut egui::Ui, quality: &mut Quality) -> bool {
    let previous = *quality;
    ui.horizontal(|ui| {
        let width = (ui.available_width() - ui.spacing().item_spacing.x * 3.0) / 4.0;
        for kbps in Quality::RATES {
            if ui
                .add_sized(
                    [width, super::theme::MENU_HEIGHT],
                    egui::Button::new(kbps.to_string()).selected(quality.kbps == kbps),
                )
                .on_hover_text(label(kbps))
                .clicked()
            {
                quality.kbps = kbps;
            }
        }
    });
    *quality != previous
}
