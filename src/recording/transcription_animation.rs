use ratatui::prelude::*;

#[derive(Clone, Debug)]
struct AsciiArtChar {
    x: f32,
    top: &'static str,
    bottom: &'static str,
    width: usize,
}

pub struct TranscriptionAnimation {
    chars: Vec<AsciiArtChar>,
    phase: f32,
    frame_count: u32,
}

impl Default for TranscriptionAnimation {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptionAnimation {
    pub fn new() -> Self {
        Self {
            chars: Vec::new(),
            phase: 0.0,
            frame_count: 0,
        }
    }

    fn init_chars(&mut self) {
        self.chars.clear();
        self.chars = [
            AsciiArtChar {
                x: 0.0,
                top: "┏┓",
                bottom: "┗┛",
                width: 2,
            },
            AsciiArtChar {
                x: 0.0,
                top: "┏",
                bottom: "┛",
                width: 1,
            },
            AsciiArtChar {
                x: 0.0,
                top: "╋",
                bottom: "┗",
                width: 1,
            },
            AsciiArtChar {
                x: 0.0,
                top: "╋",
                bottom: "┗",
                width: 1,
            },
        ]
        .to_vec();
    }

    pub fn update(&mut self) {
        self.frame_count = self.frame_count.wrapping_add(1);
    }

    fn update_chars(&mut self, width: u16, _height: u16) {
        if self.chars.is_empty() {
            self.init_chars();
        }

        self.phase = (self.frame_count as f32 * 0.03) % 1.0;

        let total_logo_width: i32 = self.chars.iter().map(|char| char.width as i32).sum();
        let center_x = (width as i32 / 2) - (total_logo_width / 2);

        let slide_in_end = 0.35;
        let pause_end = 0.60;
        let slide_out_end = 0.95;
        let total_chars = self.chars.len() as f32;
        let phase_per_char_in = slide_in_end / total_chars;
        let phase_per_char_out = (slide_out_end - pause_end) / total_chars;

        let mut current_target_x = center_x;

        for (char_idx, anim_char) in self.chars.iter_mut().enumerate() {
            let char_idx_f = char_idx as f32;
            let slide_in_start = char_idx_f * phase_per_char_in;
            let char_slide_in_end = slide_in_start + phase_per_char_in;
            let slide_out_start = pause_end + char_idx_f * phase_per_char_out;
            let char_slide_out_end = slide_out_start + phase_per_char_out;

            if self.phase >= slide_in_start && self.phase < char_slide_in_end {
                let progress = (self.phase - slide_in_start) / phase_per_char_in;
                let start_x = width as i32 + 5;
                let target_x = current_target_x;
                anim_char.x = start_x as f32 - (start_x - target_x) as f32 * progress;
            } else if self.phase >= slide_out_start && self.phase < char_slide_out_end {
                let progress = (self.phase - slide_out_start) / phase_per_char_out;
                let target_x = current_target_x;
                let end_x = -5;
                anim_char.x = target_x as f32 - (target_x - end_x) as f32 * progress;
            } else if self.phase < slide_in_start {
                anim_char.x = (width as i32 + 5) as f32;
            } else if self.phase >= char_slide_out_end {
                anim_char.x = -5.0;
            } else {
                anim_char.x = current_target_x as f32;
            }

            current_target_x += anim_char.width as i32;
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width;
        let height = area.height;

        self.update_chars(width, height);

        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                frame
                    .buffer_mut()
                    .set_string(x, y, " ", Style::default().bg(Color::Rgb(0, 0, 0)));
            }
        }

        let center_y = height / 2;
        let color = Color::Rgb(255, 255, 255);

        for anim_char in &self.chars {
            let x = anim_char.x.round() as i32;
            let char_width = anim_char.width as i32;
            let y_top = (center_y as i32) - 1;
            if x >= 0 && x + char_width <= width as i32 && y_top >= 0 && y_top < height as i32 {
                frame.buffer_mut().set_string(
                    area.x + x as u16,
                    area.y + y_top as u16,
                    anim_char.top,
                    Style::default().fg(color).bold(),
                );
            }

            let y_bottom = center_y as i32;
            if x >= 0 && x + char_width <= width as i32 && y_bottom >= 0 && y_bottom < height as i32
            {
                frame.buffer_mut().set_string(
                    area.x + x as u16,
                    area.y + y_bottom as u16,
                    anim_char.bottom,
                    Style::default().fg(color).bold(),
                );
            }
        }
    }
}
