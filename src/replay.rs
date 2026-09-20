use vte::{Params, Perform};

#[derive(Default)]
pub struct Filter {
    parser: vte::Parser,
    screen: Display,
}
impl Filter {
    pub fn feed(&mut self, text: &str) -> Vec<u8> {
        self.parser.advance(&mut self.screen, text.as_bytes());
        std::mem::take(&mut self.screen.0)
    }
}
#[derive(Default)]
struct Display(Vec<u8>);
impl Perform for Display {
    fn print(&mut self, c: char) {
        if !c.is_control() {
            self.0
                .extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        }
    }
    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\r' | b'\n' | b'\t' | 8) {
            self.0.push(byte);
        }
    }
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let allowed = if intermediates.is_empty() {
            matches!(
                action,
                'm' | 'A'
                    | 'B'
                    | 'C'
                    | 'D'
                    | 'E'
                    | 'F'
                    | 'G'
                    | 'H'
                    | 'f'
                    | 'd'
                    | 'e'
                    | 'a'
                    | '`'
                    | 'J'
                    | 'K'
                    | '@'
                    | 'P'
                    | 'X'
                    | 'L'
                    | 'M'
                    | 'S'
                    | 'T'
                    | 'r'
                    | 's'
                    | 'u'
            )
        } else {
            intermediates == b"?"
                && matches!(action, 'h' | 'l')
                && params
                    .iter()
                    .all(|p| p.len() == 1 && matches!(p[0], 6 | 7 | 25 | 47 | 1047 | 1048 | 1049))
        };
        if !allowed {
            return;
        }
        self.0.extend_from_slice(b"\x1b[");
        self.0.extend_from_slice(intermediates);
        for (index, group) in params.iter().enumerate() {
            if index > 0 {
                self.0.push(b';');
            }
            for (sub, value) in group.iter().enumerate() {
                if sub > 0 {
                    self.0.push(b':');
                }
                self.0.extend_from_slice(value.to_string().as_bytes());
            }
        }
        self.0.push(action as u8);
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && matches!(byte, b'7' | b'8' | b'D' | b'E' | b'M') {
            self.0.extend_from_slice(&[0x1b, byte]);
        }
    }
    // OSC, DCS, APC and other control strings have no allowed side effects.
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allow_display_but_not_terminal_side_effects() {
        let mut filter = Filter::default();
        let text = "a\x1b]52;c;clipboard\x07\x1b]0;title\x1b\\\x1bP+qquery\x1b\\\x1b_hidden\x1b\\\x1b[8;40;100t\x1b[6n\x1b[?1000h\x1bc\x07\x1b[31mred\x1b[0m\x1b[2D\x1b[Kb";
        assert_eq!(filter.feed(text), b"a\x1b[31mred\x1b[0m\x1b[2D\x1b[0Kb");
    }
    #[test]
    fn filter_sequences_across_events() {
        let mut filter = Filter::default();
        assert_eq!(filter.feed("start\x1b]5"), b"start");
        assert!(filter.feed("2;c;hidden\x1b").is_empty());
        assert_eq!(filter.feed("\\end\x1b[3"), b"end");
        assert_eq!(filter.feed("2mgreen"), b"\x1b[32mgreen");
    }
}
