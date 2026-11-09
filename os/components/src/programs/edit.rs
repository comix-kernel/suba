//! 串口全屏文本编辑器。光标始终落在 UTF-8 边界，缓冲区不超过一次文件替换的上限。
use super::Environment;
use crate::terminal::Terminal;
use alloc::{format, string::String};
use core::fmt::Write;
use service_api::{Error, File, Mode, Name, Result, MAX_DATA};

const ROWS: usize = 12;
const WIDTH: usize = 70;

#[derive(Clone, Copy, PartialEq)]
enum Key {
    Char(char),
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Backspace,
    Delete,
    Save,
    Quit,
    Other,
}

// 原始串口字节由前台任务独占。与 shell 共用 CR/LF 折叠状态。
fn key(term: &Terminal) -> Result<Key> {
    let mut b = term.byte()?.ok_or(Error::Closed)?;
    if b == 27 {
        b = term.byte()?.ok_or(Error::Closed)?;
        if matches!(b, b'[' | b'O') {
            let mut parameter = 0u8;
            loop {
                b = term.byte()?.ok_or(Error::Closed)?;
                if b.is_ascii_digit() {
                    parameter = parameter.saturating_mul(10).saturating_add(b - b'0');
                } else if (0x40..=0x7e).contains(&b) {
                    return Ok(match (b, parameter) {
                        (b'A', _) => Key::Up,
                        (b'B', _) => Key::Down,
                        (b'C', _) => Key::Right,
                        (b'D', _) => Key::Left,
                        (b'H', _) | (b'~', 1 | 7) => Key::Home,
                        (b'F', _) | (b'~', 4 | 8) => Key::End,
                        (b'~', 3) => Key::Delete,
                        _ => Key::Other,
                    });
                }
            }
        }
    }
    Ok(match b {
        19 => Key::Save,
        17 => Key::Quit,
        8 | 127 => Key::Backspace,
        b'\n' | b'\t' | 32..=126 => Key::Char(b as char),
        0xc2..=0xf4 => {
            let len = if b < 0xe0 {
                2
            } else if b < 0xf0 {
                3
            } else {
                4
            };
            let mut bytes = [b; 4];
            for byte in &mut bytes[1..len] {
                *byte = term.byte()?.ok_or(Error::Closed)?;
            }
            core::str::from_utf8(&bytes[..len])
                .ok()
                .and_then(|s| s.chars().next())
                .filter(|c| !c.is_control())
                .map_or(Key::Other, Key::Char)
        }
        _ => Key::Other,
    })
}

struct Buffer {
    text: String,
    pos: usize,
}

impl Buffer {
    fn line_start(&self) -> usize {
        self.text[..self.pos].rfind('\n').map_or(0, |i| i + 1)
    }
    fn line_end(&self) -> usize {
        self.pos
            + self.text[self.pos..]
                .find('\n')
                .unwrap_or(self.text.len() - self.pos)
    }
    fn previous(&self) -> usize {
        self.text[..self.pos]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }
    fn next(&self) -> usize {
        self.pos
            + self.text[self.pos..]
                .chars()
                .next()
                .map_or(0, char::len_utf8)
    }

    fn apply(&mut self, key: Key) -> Result<()> {
        match key {
            Key::Char(c) => {
                if self.text.len() + c.len_utf8() > MAX_DATA {
                    return Err(Error::NoCapacity);
                }
                self.text.insert(self.pos, c);
                self.pos += c.len_utf8();
            }
            Key::Left => self.pos = self.previous(),
            Key::Right => self.pos = self.next(),
            Key::Home => self.pos = self.line_start(),
            Key::End => self.pos = self.line_end(),
            Key::Backspace => {
                let before = self.previous();
                self.text.replace_range(before..self.pos, "");
                self.pos = before;
            }
            Key::Delete => self.text.replace_range(self.pos..self.next(), ""),
            Key::Up | Key::Down => {
                let start = self.line_start();
                let col = self.text[start..self.pos].chars().count();
                let target = if key == Key::Up {
                    if start == 0 {
                        return Ok(());
                    }
                    self.text[..start - 1].rfind('\n').map_or(0, |i| i + 1)
                } else {
                    let end = self.line_end();
                    if end == self.text.len() {
                        return Ok(());
                    }
                    end + 1
                };
                let line = self.text[target..].split('\n').next().unwrap();
                self.pos = target + line.char_indices().nth(col).map_or(line.len(), |(i, _)| i);
            }
            _ => {}
        }
        Ok(())
    }

    fn draw(
        &self,
        term: &Terminal,
        path: &str,
        dirty: bool,
        message: &str,
        top: &mut usize,
    ) -> Result<()> {
        let row = self.text[..self.pos]
            .bytes()
            .filter(|&b| b == b'\n')
            .count();
        let prefix = &self.text[self.line_start()..self.pos];
        // 用字节数保守限定可见宽度；实际列位置由终端渲染光标前的文字决定。
        let mut left = prefix.len().saturating_sub(WIDTH - 1);
        while !prefix.is_char_boundary(left) {
            left += 1;
        }
        let skip = prefix[..left].chars().count();
        *top = (*top).clamp((row + 1).saturating_sub(ROWS), row);
        let mut screen = format!(
            "\x1b[?25l\x1b[HSuba Edit   {path}{}\x1b[K",
            if dirty { "  *" } else { "" }
        );
        let mut lines = self.text.split('\n').skip(*top);
        for i in 0..ROWS {
            write!(screen, "\x1b[{};1H", i + 2).unwrap();
            let mut bytes = 0;
            for c in lines.next().unwrap_or("").chars().skip(skip) {
                bytes += c.len_utf8();
                if bytes > WIDTH {
                    break;
                }
                screen.push(display(c));
            }
            screen.push_str("\x1b[K");
        }
        write!(screen, "\x1b[{};1H^S Save    ^Q Quit    Ln {}  Col {}\x1b[K\x1b[{};1H{message}\x1b[K\x1b[{};1H",
            ROWS+2, row+1, prefix.chars().count()+1, ROWS+3, row-*top+2).unwrap();
        screen.extend(prefix[left..].chars().map(display));
        screen.push_str("\x1b[?25h");
        term.print(&screen)
    }
}

fn display(c: char) -> char {
    if c == '\t' {
        ' '
    } else if c.is_control() {
        '·'
    } else {
        c
    }
}

fn edit(term: &Terminal, path: &str, file: &File, original: String, created: bool) -> Result<()> {
    let mut buffer = Buffer {
        text: original.clone(),
        pos: 0,
    };
    let mut saved = original;
    let mut top = 0;
    let mut confirm = false;
    let mut message: String = if created {
        "New file"
    } else {
        "Arrow keys move; type to edit."
    }
    .into();
    loop {
        let dirty = buffer.text != saved;
        buffer.draw(term, path, dirty, &message, &mut top)?;
        let k = key(term)?;
        if k == Key::Quit {
            if !dirty || confirm {
                return Ok(());
            }
            confirm = true;
            message = "Unsaved changes. ^S to save, or ^Q again to discard.".into();
            continue;
        }
        confirm = false;
        message.clear();
        if k == Key::Save {
            match file.replace(buffer.text.as_bytes()) {
                Ok(()) => {
                    saved.clone_from(&buffer.text);
                    message = format!("Saved {} bytes", saved.len());
                }
                Err(e) => message = format!("Save failed: {e:?}"),
            }
        } else if buffer.apply(k).is_err() {
            message = format!("File limit: {MAX_DATA} bytes");
        }
    }
}

pub(super) fn run(env: &Environment, path: &str) -> Result<String> {
    if path.is_empty() {
        return Ok("Usage: edit /work/FILE".into());
    }
    let (name, sub) = env.view.resolve(path)?;
    if !matches!(name, Name::Store | Name::Record) || sub.is_empty() {
        return Ok("edit: open a regular memory file, e.g. /work/note".into());
    }
    let (mut file, created) = match env.view.open(env.access.as_ref(), path, Mode::ReadWrite) {
        Ok(f) => (f, false),
        Err(Error::NotFound) => (
            env.view
                .create(env.access.as_ref(), path, Mode::ReadWrite)?,
            true,
        ),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8(file.read(MAX_DATA)?).map_err(|_| Error::Invalid)?;
    env.terminal.print("\x1b[?1049h\x1b[2J")?;
    let result = edit(&env.terminal, path, &file, text, created);
    env.terminal.print("\x1b[?25h\x1b[?1049l")?;
    file.close()?;
    result.map(|()| String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_and_deletion_follow_character_boundaries() {
        let mut b = Buffer {
            text: "ab\n中文".into(),
            pos: 1,
        };
        b.apply(Key::Down).unwrap();
        b.apply(Key::Backspace).unwrap();
        assert_eq!(b.text, "ab\n文");
        b.apply(Key::Backspace).unwrap();
        b.apply(Key::Delete).unwrap();
        assert_eq!(b.text, "ab");
        b.apply(Key::Char('\n')).unwrap();
        b.apply(Key::Up).unwrap();
        assert_eq!(b.pos, 0);
    }

    #[test]
    fn full_document_rejects_insert_without_losing_text() {
        let mut b = Buffer {
            text: "x".repeat(MAX_DATA),
            pos: MAX_DATA,
        };
        assert_eq!(b.apply(Key::Char('文')), Err(Error::NoCapacity));
        assert_eq!(b.pos, MAX_DATA);
        b.apply(Key::Backspace).unwrap();
        b.apply(Key::Char('a')).unwrap();
        assert_eq!(b.text.len(), MAX_DATA);
    }
}
