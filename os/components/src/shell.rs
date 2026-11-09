//! 交互任务通过自己的挂载视图访问文件；服务替换也通过管理文件完成。
use super::*;
use alloc::{format, sync::Arc};

use crate::{
    programs,
    terminal::{History, Line, Terminal},
};

pub fn run(
    mut view: Namespace,
    access: impl Connect + 'static,
    admin: impl Admin + 'static,
) -> Result<()> {
    let access: Arc<dyn Connect> = Arc::new(access);
    let admin: Arc<dyn Admin> = Arc::new(admin);
    let terminal = Arc::new(Terminal::open(&view, access.as_ref())?);
    let mut held = None;
    let mut history = History::default();
    terminal.print(&programs::menu())?;
    loop {
        let line = match terminal.readline("comix> ", &mut history)? {
            Line::Text(line) => line,
            Line::Cancel => continue,
            Line::End => admin.shutdown(),
        };
        let msg = command(
            line.trim(),
            &mut view,
            &access,
            &admin,
            &terminal,
            &mut held,
        )
        .unwrap_or_else(|e| format!("error {e:?}"));
        if !msg.is_empty() {
            terminal.print(&msg)?;
            terminal.print("\n")?;
        }
    }
}

fn command(
    text: &str,
    view: &mut Namespace,
    access: &Arc<dyn Connect>,
    admin: &Arc<dyn Admin>,
    terminal: &Arc<Terminal>,
    held: &mut Option<File>,
) -> Result<String> {
    let (cmd, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let arg = rest.trim_start();
    if let Some(program) = programs::find(cmd) {
        let hart = admin.hart();
        let inherited = view.clone();
        let access = access.clone();
        let terminal = terminal.clone();
        let child = admin.clone();
        let args = arg.to_string();
        return admin.run_on(
            hart,
            Box::new(move || {
                let env = programs::Environment {
                    view: inherited,
                    access,
                    terminal,
                    task: child.task_id(),
                };
                (program.main)(&env, &args)
            }),
        );
    }
    match cmd {
        "" => Ok(String::new()),
        "apps" | "menu" => Ok(programs::menu()),
        "help" => Ok(concat!(
            "Programs    apps  |  hello [NAME]  |  ps  |  calc [EXPR]  |  wc PATH  |  edit PATH\n",
            "Files       ls PATH  |  cat PATH  |  create PATH  |  put PATH TEXT\n",
            "Handles     open PATH  |  read  |  close\n",
            "Services    mount PREFIX store|record|counter|console  |  replace  |  migrate\n",
            "Tasks       id  |  on HART COMMAND[;COMMAND]\n",
            "Terminal    Up/Down history  |  Ctrl-U erase line  |  halt"
        )
        .into()),
        "id" => Ok(admin.task_id()),
        "on" => {
            let (hart, script) = arg.split_once(' ').ok_or(Error::Invalid)?;
            let hart = hart.parse().map_err(|_| Error::Invalid)?;
            let mut inherited = view.clone();
            let access = access.clone();
            let child_admin = admin.clone();
            let script = script.to_string();
            let terminal = terminal.clone();
            admin.run_on(
                hart,
                Box::new(move || {
                    let mut held = None;
                    let mut lines = Vec::new();
                    for text in script.split(';') {
                        let value = command(
                            text.trim(),
                            &mut inherited,
                            &access,
                            &child_admin,
                            &terminal,
                            &mut held,
                        )?;
                        if !value.is_empty() {
                            lines.push(value)
                        }
                    }
                    Ok(lines.join("\n"))
                }),
            )
        }
        "create" => {
            view.create(access.as_ref(), arg, Mode::Write)?.close()?;
            Ok("created".into())
        }
        "ls" | "cat" => {
            let mut f = view.open(access.as_ref(), arg, Mode::Read)?;
            let mut data = Vec::new();
            loop {
                let chunk = f.read(MAX_DATA)?;
                let short = chunk.len() < MAX_DATA;
                data.extend_from_slice(&chunk);
                if short {
                    break;
                }
            }

            f.close()?;
            Ok(String::from_utf8_lossy(&data).into_owned())
        }
        "put" => {
            let (path, data) = arg.split_once(' ').ok_or(Error::Invalid)?;
            let mut f = view.open(access.as_ref(), path, Mode::Write)?;
            let n = f.write(data.as_bytes())?;
            f.close()?;
            Ok(format!("written {n}"))
        }
        "open" => {
            if held.is_some() {
                return Err(Error::Busy);
            }
            *held = Some(view.open(access.as_ref(), arg, Mode::Read)?);
            Ok("opened".into())
        }
        "read" => {
            let data = held.as_mut().ok_or(Error::Closed)?.read(MAX_DATA)?;
            Ok(String::from_utf8_lossy(&data).into_owned())
        }
        "close" => {
            held.take().ok_or(Error::Closed)?.close()?;
            Ok("closed".into())
        }
        "mount" => {
            let (prefix, name) = arg.split_once(' ').ok_or(Error::Invalid)?;
            let name = match name {
                "store" => Name::Store,
                "record" => Name::Record,
                "console" => Name::Console,
                "counter" => Name::Counter,
                _ => return Err(Error::NotFound),
            };
            *view = view.mount(prefix, name)?;
            Ok("mounted".into())
        }
        "replace" | "migrate" => {
            let mut ctl = view.open(access.as_ref(), "/svc/store/ctl", Mode::Write)?;
            ctl.write(cmd.as_bytes())?;
            ctl.close()?;
            let mut file = view.open(access.as_ref(), "/svc/store/generation", Mode::Read)?;
            let generation = file.read(32)?;
            file.close()?;
            Ok(format!(
                "generation {}",
                String::from_utf8_lossy(&generation)
            ))
        }
        "halt" => admin.shutdown(),
        _ => Ok(programs::unknown(cmd)),
    }
}
