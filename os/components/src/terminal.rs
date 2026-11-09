//! 串口前台。shell 等待子任务退出时，将同一个终端交给交互程序。
use alloc::{collections::VecDeque, string::String, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use service_api::*;

pub(crate) struct Terminal {
    input: File,
    output: File,
    after_cr: AtomicBool,
}
pub(crate) enum Line {
    Text(String),
    Cancel,
    End,
}

#[derive(Default)]
pub(crate) struct History(VecDeque<String>);
impl History {
    fn remember(&mut self, line: &str) {
        if line.is_empty() || self.0.back().is_some_and(|last| last == line) {
            return;
        }
        if self.0.len() == 8 {
            self.0.pop_front();
        }
        self.0.push_back(line.into());
    }
}

impl Terminal {
    pub fn open(ns: &Namespace, access: &dyn Connect) -> Result<Self> {
        Ok(Self {
            input: ns.open(access, "/dev/cons", Mode::Read)?,
            output: ns.open(access, "/dev/cons", Mode::Write)?,
            after_cr: AtomicBool::new(false),
        })
    }
    fn send(&self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            let n = self
                .output
                .write_at(0, &bytes[..bytes.len().min(MAX_DATA)])?;
            if n == 0 {
                return Err(Error::Closed);
            }
            bytes = &bytes[n..];
        }
        Ok(())
    }
    pub fn print(&self, text: &str) -> Result<()> {
        for part in text.split_inclusive('\n') {
            if let Some(line) = part.strip_suffix('\n') {
                self.send(line.trim_end_matches('\r').as_bytes())?;
                self.send(b"\r\n")?;
            } else {
                self.send(part.as_bytes())?;
            }
        }
        Ok(())
    }
    pub fn byte(&self) -> Result<Option<u8>> {
        loop {
            let bytes = self.input.read_at(0, 1)?;
            let Some(&b) = bytes.first() else {
                return Ok(None);
            };
            if self.after_cr.swap(b == b'\r', Ordering::Relaxed) && b == b'\n' {
                continue;
            }
            return Ok(Some(if b == b'\r' { b'\n' } else { b }));
        }
    }

    fn redraw(&self, prompt: &str, line: &[u8]) -> Result<()> {
        self.send(b"\r\x1b[2K")?;
        self.print(prompt)?;
        self.send(line)
    }

    pub fn readline(&self, prompt: &str, history: &mut History) -> Result<Line> {
        self.print(prompt)?;
        let mut line = Vec::new();
        let mut draft = Vec::new();
        let mut index = history.0.len();
        let mut escape = 0;
        loop {
            let Some(b) = self.byte()? else {
                return Ok(Line::End);
            };
            if escape == 1 {
                escape = if matches!(b, b'[' | b'O') { 2 } else { 0 };
                if escape == 2 {
                    continue;
                }
            } else if escape == 2 {
                if !(0x40..=0x7e).contains(&b) {
                    continue;
                }
                escape = 0;
                match b {
                    b'A' if index > 0 => {
                        if index == history.0.len() {
                            draft.clone_from(&line);
                        }
                        index -= 1;
                    }
                    b'B' if index < history.0.len() => index += 1,
                    _ => continue,
                }
                line = history
                    .0
                    .get(index)
                    .map_or_else(|| draft.clone(), |s| s.as_bytes().to_vec());
                self.redraw(prompt, &line)?;
                continue;
            }
            match b {
                27 => escape = 1,
                b'\r' | b'\n' => {
                    self.print("\n")?;
                    let text = String::from_utf8(line).expect("ASCII input");
                    history.remember(&text);
                    return Ok(Line::Text(text));
                }
                3 => {
                    self.print("^C\n")?;
                    return Ok(Line::Cancel);
                }
                4 if line.is_empty() => {
                    self.print("\n")?;
                    return Ok(Line::End);
                }
                21 => {
                    line.clear();
                    index = history.0.len();
                    self.redraw(prompt, &line)?;
                }
                8 | 127 if !line.is_empty() => {
                    line.pop();
                    index = history.0.len();
                    self.send(b"\x08 \x08")?;
                }
                32..=126 if line.len() < 240 => {
                    line.push(b);
                    index = history.0.len();
                    self.send(&[b])?;
                }
                _ => {}
            }
        }
    }
}
