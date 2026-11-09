//! 检查溢出的整数计算器。表达式与交互模式使用同一个求值器。
use super::Environment;
use crate::terminal::{History, Line};
use alloc::{
    format,
    string::{String, ToString},
};
use service_api::Result;

type Math<T> = core::result::Result<T, &'static str>;
const OVERFLOW: &str = "integer overflow";

struct Parser<'a>(&'a str);
impl Parser<'_> {
    fn eat(&mut self, token: char) -> bool {
        self.0 = self.0.trim_start();
        if let Some(rest) = self.0.strip_prefix(token) {
            self.0 = rest;
            true
        } else {
            false
        }
    }
    fn atom(&mut self) -> Math<i64> {
        let mut negative = false;
        loop {
            if self.eat('-') {
                negative = !negative;
            } else if !self.eat('+') {
                break;
            }
        }
        if self.eat('(') {
            let value = self.sum()?;
            if !self.eat(')') {
                return Err("missing ')'");
            }
            return if negative {
                value.checked_neg().ok_or(OVERFLOW)
            } else {
                Ok(value)
            };
        }
        let n = self.0.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return Err("expected an integer or '('");
        }
        let (digits, rest) = self.0.split_at(n);
        self.0 = rest;
        let mut value = 0i64;
        for b in digits.bytes() {
            let digit = i64::from(b - b'0');
            let scaled = value.checked_mul(10).ok_or(OVERFLOW)?;
            value = if negative {
                scaled.checked_sub(digit)
            } else {
                scaled.checked_add(digit)
            }
            .ok_or(OVERFLOW)?;
        }
        Ok(value)
    }
    fn product(&mut self) -> Math<i64> {
        let mut n = self.atom()?;
        loop {
            if self.eat('*') {
                n = n.checked_mul(self.atom()?).ok_or(OVERFLOW)?;
            } else if self.eat('/') {
                let divisor = self.atom()?;
                if divisor == 0 {
                    return Err("division by zero");
                }
                n = n.checked_div(divisor).ok_or(OVERFLOW)?;
            } else {
                return Ok(n);
            }
        }
    }
    fn sum(&mut self) -> Math<i64> {
        let mut n = self.product()?;
        loop {
            if self.eat('+') {
                n = n.checked_add(self.product()?).ok_or(OVERFLOW)?;
            } else if self.eat('-') {
                n = n.checked_sub(self.product()?).ok_or(OVERFLOW)?;
            } else {
                return Ok(n);
            }
        }
    }
}
fn evaluate(text: &str) -> String {
    let mut parser = Parser(text);
    match parser.sum() {
        Ok(n) if parser.0.trim().is_empty() => n.to_string(),
        Ok(_) => "calc: unexpected input".into(),
        Err(message) => format!("calc: {message}"),
    }
}
pub(super) fn run(env: &Environment, args: &str) -> Result<String> {
    if !args.is_empty() {
        return Ok(evaluate(args));
    }
    env.terminal.print("Integer calculator  (+ - * / and parentheses)\nEnter an expression, or q to return to the shell.\n")?;
    let mut history = History::default();
    while let Line::Text(text) = env.terminal.readline("calc> ", &mut history)? {
        match text.trim() {
            "q" | "quit" | "exit" => break,
            "" => continue,
            expr => env.terminal.print(&format!("{}\n", evaluate(expr)))?,
        }
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::evaluate;
    #[test]
    fn precedence_and_errors() {
        for (input, output) in [
            (" 2 + 3 * (4 - 1) ", "11"),
            ("18 / 3 / 2", "3"),
            ("2 * -(2 + 3)", "-10"),
            ("-9223372036854775808", "-9223372036854775808"),
            ("9223372036854775807 + 1", "calc: integer overflow"),
            ("1 / 0", "calc: division by zero"),
            ("1 2", "calc: unexpected input"),
            ("(3 + 4", "calc: missing ')'"),
        ] {
            assert_eq!(evaluate(input), output);
        }
    }
}
