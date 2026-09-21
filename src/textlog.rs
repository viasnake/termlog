use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset, SecondsFormat};
use std::io::{BufRead, Lines};

#[derive(Debug, PartialEq)]
pub struct Line {
    pub time: DateTime<FixedOffset>,
    pub text: String,
}

pub struct Format {
    version: u32,
    date: String,
}
impl Format {
    pub fn new(version: u32) -> Self {
        Self {
            version,
            date: String::new(),
        }
    }
    pub fn line(&mut self, line: &Line) -> String {
        if self.version == 1 {
            return format!(
                "{}\t{}\n",
                line.time.to_rfc3339_opts(SecondsFormat::Micros, false),
                line.text
            );
        }
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
            line.time.format("%H:%M:%S"),
            line.text
        ));
        output
    }
}

pub struct Reader<R: BufRead> {
    lines: std::iter::Enumerate<Lines<R>>,
    version: u32,
    date: Option<(String, String)>,
}
impl<R: BufRead> Reader<R> {
    pub fn new(reader: R, version: u32) -> Result<Self> {
        anyhow::ensure!(
            matches!(version, 1 | 2),
            "unsupported transcript version: {version}"
        );
        Ok(Self {
            lines: reader.lines().enumerate(),
            version,
            date: None,
        })
    }
    pub fn next(&mut self) -> Result<Option<(usize, Line)>> {
        for (index, text) in self.lines.by_ref() {
            let text = text?;
            if self.version == 1 {
                let (time, body) = text
                    .split_once('\t')
                    .context("invalid legacy transcript line")?;
                return Ok(Some((
                    index + 1,
                    Line {
                        time: DateTime::parse_from_rfc3339(time)?,
                        text: body.into(),
                    },
                )));
            }
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
            if text.get(8..10) != Some("  ") {
                bail!("invalid transcript line {}", index + 1);
            }
            let (date, offset) = self
                .date
                .as_ref()
                .context("transcript date heading is missing")?;
            let time = DateTime::parse_from_rfc3339(&format!("{date}T{}{offset}", &text[..8]))?;
            return Ok(Some((
                index + 1,
                Line {
                    time,
                    text: text[10..].into(),
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
    fn readable_round_trip_keeps_every_time_and_body() {
        let mut format = Format::new(2);
        let mut text = String::new();
        let cases = [
            ("2026-09-20T23:59:59+09:00", "日本語"),
            ("2026-09-20T23:59:59+09:00", "  indented\ttext"),
            ("2026-09-20T23:59:59+09:00", ""),
            ("2026-09-21T00:00:00+09:00", "2026-01-01 UTC+00:00"),
        ];
        for (time, body) in cases {
            text.push_str(&format.line(&Line {
                time: DateTime::parse_from_rfc3339(time).unwrap(),
                text: body.into(),
            }));
        }
        assert_eq!(text, "2026-09-20 UTC+09:00\n\n23:59:59  日本語\n23:59:59    indented\ttext\n23:59:59  \n\n2026-09-21 UTC+09:00\n\n00:00:00  2026-01-01 UTC+00:00\n");
        let mut reader = Reader::new(text.as_bytes(), 2).unwrap();
        for (time, body) in cases {
            let (_, line) = reader.next().unwrap().unwrap();
            assert_eq!(line.time, DateTime::parse_from_rfc3339(time).unwrap());
            assert_eq!(line.text, body);
        }
        assert!(reader.next().unwrap().is_none());
        assert!(Reader::new(b"12:34:56  missing date\n".as_slice(), 2)
            .unwrap()
            .next()
            .is_err());
    }
    #[test]
    fn legacy_format_keeps_fractional_timestamps() {
        let original = "2026-09-20T12:34:56.123456+09:00\t  日本語\ttext\n";
        let (_, line) = Reader::new(original.as_bytes(), 1)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(Format::new(1).line(&line), original);
        assert_eq!(
            Format::new(2).line(&line),
            "2026-09-20 UTC+09:00\n\n12:34:56    日本語\ttext\n"
        );
    }
}
