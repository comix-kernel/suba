//! 镜像内程序目录。每个入口在新任务栈上运行，继承启动者的命名空间。
mod calc;
mod edit;
mod tools;

use crate::terminal::Terminal;
use alloc::{format, string::String, sync::Arc};
use core::fmt::Write;
use service_api::{Connect, Namespace, Result};

pub(crate) struct Environment {
    pub view: Namespace,
    pub access: Arc<dyn Connect>,
    pub terminal: Arc<Terminal>,
    pub task: String,
}

pub(crate) struct Program {
    pub name: &'static str,
    usage: &'static str,
    summary: &'static str,
    pub main: fn(&Environment, &str) -> Result<String>,
}

const PROGRAMS: &[Program] = &[
    Program {
        name: "hello",
        usage: "hello [NAME]",
        summary: "Meet the task running this program",
        main: tools::hello,
    },
    Program {
        name: "ps",
        usage: "ps",
        summary: "List live tasks on both harts",
        main: tools::ps,
    },
    Program {
        name: "calc",
        usage: "calc [EXPR]",
        summary: "Open the calculator or evaluate an expression",
        main: calc::run,
    },
    Program {
        name: "wc",
        usage: "wc [PATH]",
        summary: "Count lines, words and bytes in a file",
        main: tools::wc,
    },
    Program {
        name: "edit",
        usage: "edit PATH",
        summary: "Edit a text file in /work",
        main: edit::run,
    },
];

pub(crate) fn find(name: &str) -> Option<&'static Program> {
    if let Ok(n) = name.parse::<usize>() {
        return n.checked_sub(1).and_then(|i| PROGRAMS.get(i));
    }
    PROGRAMS.iter().find(|p| p.name == name)
}

pub(crate) fn menu() -> String {
    let mut text = String::from("\n  SUBA  /  SERIAL SHELL\n\n  Programs\n");
    for (i, p) in PROGRAMS.iter().enumerate() {
        writeln!(text, "   {}   {:<14} {}", i + 1, p.usage, p.summary).unwrap();
    }
    text.push_str("\n  Type a name or number and press Enter. Try: calc (7 + 5) * 3\n  help  Commands     apps  Programs     halt  Power off\n");
    text
}
pub(super) fn unknown(name: &str) -> String {
    format!("Unknown command '{name}'. Type apps for programs or help for commands.")
}
