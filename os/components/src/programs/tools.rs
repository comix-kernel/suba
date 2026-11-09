use super::Environment;
use alloc::{format, string::String, vec::Vec};
use core::fmt::Write;
use service_api::{Error, Mode, Result, MAX_DATA};

pub(super) fn hello(env: &Environment, args: &str) -> Result<String> {
    let name = if args.is_empty() { "world" } else { args };
    Ok(format!("Hello, {name}!\nMy task: {}", env.task))
}

fn read_file(env: &Environment, path: &str) -> Result<Vec<u8>> {
    let mut f = env.view.open(env.access.as_ref(), path, Mode::Read)?;
    let data = f.read(MAX_DATA)?;
    f.close()?;
    Ok(data)
}

pub(super) fn ps(env: &Environment, args: &str) -> Result<String> {
    if !args.is_empty() {
        return Ok("Usage: ps".into());
    }
    let mut rows = String::from("HART  TASK       STATE");
    for hart in 0..2 {
        let root = format!("/tasks/{hart}");
        let list = read_file(env, &root)?;
        for id in core::str::from_utf8(&list)
            .map_err(|_| Error::Invalid)?
            .lines()
        {
            let path = format!("{root}/{id}/status");
            // 枚举后任务可以退出，代号检查会让这次打开返回 NotFound。
            let snapshot = match read_file(env, &path) {
                Err(Error::NotFound) => continue,
                result => result?,
            };
            let state = core::str::from_utf8(&snapshot)
                .map_err(|_| Error::Invalid)?
                .split_ascii_whitespace()
                .last()
                .ok_or(Error::Invalid)?;
            write!(rows, "\n{hart:<6}{id:<11}{state}").unwrap();
            if path == env.task {
                rows.push_str("  (ps)");
            }
        }
    }
    Ok(rows)
}

pub(super) fn wc(env: &Environment, path: &str) -> Result<String> {
    let path = if path.is_empty() { "/svc/memory" } else { path };
    let mut file = env.view.open(env.access.as_ref(), path, Mode::Read)?;
    let (mut lines, mut words, mut bytes) = (0usize, 0usize, 0usize);
    let mut in_word = false;
    loop {
        let chunk = file.read(256)?;
        if chunk.is_empty() {
            break;
        }
        bytes += chunk.len();
        for b in chunk {
            lines += usize::from(b == b'\n');
            let next = !b.is_ascii_whitespace();
            words += usize::from(next && !in_word);
            in_word = next;
        }
    }
    file.close()?;
    Ok(format!(
        "{lines} lines  {words} words  {bytes} bytes  {path}"
    ))
}
