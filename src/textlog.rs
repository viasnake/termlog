use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset};
use std::io::{BufRead, Lines};

#[derive(Debug, PartialEq)]
pub struct Line {
    pub time: DateTime<FixedOffset>,
    pub text: String,
}

#[derive(Default)]
pub struct Format {
    date: String,
}
impl Format {
    pub fn line(&mut self, line: &Line) -> String {
        let date = line.time.format("%Y-%m-%d UTC%:z").to_string();
        let mut output = String::new();
        if date != self.date {
            if !self.date.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!("{date}\n\n"));
            self.date = date;
        }
        output.push_str(&format!(
            "{}  {}\n",
            line.time.format("%H:%M:%S%.3f"),
            line.text
        ));
        output
    }
}

pub struct Reader<R: BufRead> {
    lines: std::iter::Enumerate<Lines<R>>,
    date: Option<(String, String)>,
}
impl<R: BufRead> Reader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            lines: reader.lines().enumerate(),
            date: None,
        }
    }
    pub fn next(&mut self) -> Result<Option<(usize, Line)>> {
        for (index, text) in self.lines.by_ref() {
            let text = text?;
            if text.is_empty() {
                continue;
            }
            if let Some((date, offset)) = text.split_once(" UTC") {
                // Headings never start with a time; a heading inside output stays body text.
                if date.len() == 10 {
                    DateTime::parse_from_rfc3339(&format!("{date}T00:00:00{offset}"))?;
                    self.date = Some((date.into(), offset.into()));
                    continue;
                }
            }
            if text.get(12..14) != Some("  ") {
                bail!("invalid transcript line {}", index + 1);
            }
            let (date, offset) = self
                .date
                .as_ref()
                .context("transcript date heading is missing")?;
            let time = DateTime::parse_from_rfc3339(&format!("{date}T{}{offset}", &text[..12]))?;
            return Ok(Some((
                index + 1,
                Line {
                    time,
                    text: text[14..].into(),
                },
            )));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn millisecond_times_preserve_body_and_date_changes() {
        let cases = [
            ("2026-09-20T23:59:59.123+09:00", "日本語"),
            ("2026-09-20T23:59:59.456+09:00", "  indented\ttext"),
            ("2026-09-20T23:59:59.456+09:00", ""),
            ("2026-09-21T00:00:00.789+09:00", "2026-01-01 UTC+00:00"),
        ];
        let mut format = Format::default();
        let mut text = String::new();
        for (time, body) in cases {
            text.push_str(&format.line(&Line {
                time: DateTime::parse_from_rfc3339(time).unwrap(),
                text: body.into(),
            }));
        }
        assert_eq!(text, "2026-09-20 UTC+09:00\n\n23:59:59.123  日本語\n23:59:59.456    indented\ttext\n23:59:59.456  \n\n2026-09-21 UTC+09:00\n\n00:00:00.789  2026-01-01 UTC+00:00\n");
        let mut reader = Reader::new(text.as_bytes());
        for (time, body) in cases {
            let (_, line) = reader.next().unwrap().unwrap();
            assert_eq!(line.time, DateTime::parse_from_rfc3339(time).unwrap());
            assert_eq!(line.text, body);
        }
        assert!(reader.next().unwrap().is_none());
        assert!(Reader::new(b"12:34:56.789  missing date\n".as_slice())
            .next()
            .is_err());
        let precise = Line {
            time: DateTime::parse_from_rfc3339("2026-09-21T00:00:00.123999+09:00").unwrap(),
            text: "microseconds remain in cast".into(),
        };
        assert!(format.line(&precise).starts_with("00:00:00.123  "));
    }
}
