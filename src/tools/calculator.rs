//! Shunting-yard expression evaluator over f64.

use anyhow::{anyhow, Result};

pub fn eval(expr: &str) -> Result<f64> {
    let tokens = tokenize(expr)?;
    let rpn = to_rpn(tokens)?;
    eval_rpn(rpn)
}

#[derive(Debug, Clone)]
enum Token {
    Num(f64),
    Op(char),
    LParen,
    RParen,
}

fn tokenize(s: &str) -> Result<Vec<Token>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '0'..='9' | '.' => {
                let mut num = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_digit() || c == '.' {
                        num.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(Token::Num(num.parse()?));
            }
            '+' | '-' | '*' | '/' | '^' => {
                out.push(Token::Op(c));
                chars.next();
            }
            '(' => {
                out.push(Token::LParen);
                chars.next();
            }
            ')' => {
                out.push(Token::RParen);
                chars.next();
            }
            other => return Err(anyhow!("unexpected character: {other}")),
        }
    }
    Ok(out)
}

fn prec(c: char) -> u8 {
    match c {
        '+' | '-' => 1,
        '*' | '/' => 2,
        '^' => 3,
        _ => 0,
    }
}

fn right_assoc(c: char) -> bool {
    c == '^'
}

fn to_rpn(tokens: Vec<Token>) -> Result<Vec<Token>> {
    let mut out = Vec::new();
    let mut ops: Vec<Token> = Vec::new();
    for t in tokens {
        match t {
            Token::Num(_) => out.push(t),
            Token::Op(op) => {
                while let Some(top) = ops.last() {
                    if let Token::Op(top_op) = top {
                        if prec(*top_op) > prec(op)
                            || (prec(*top_op) == prec(op) && !right_assoc(op))
                        {
                            out.push(ops.pop().unwrap());
                            continue;
                        }
                    }
                    break;
                }
                ops.push(Token::Op(op));
            }
            Token::LParen => ops.push(Token::LParen),
            Token::RParen => {
                while let Some(top) = ops.pop() {
                    if matches!(top, Token::LParen) {
                        break;
                    }
                    out.push(top);
                }
            }
        }
    }
    while let Some(top) = ops.pop() {
        if matches!(top, Token::LParen | Token::RParen) {
            return Err(anyhow!("mismatched parentheses"));
        }
        out.push(top);
    }
    Ok(out)
}

fn eval_rpn(rpn: Vec<Token>) -> Result<f64> {
    let mut stack = Vec::new();
    for t in rpn {
        match t {
            Token::Num(n) => stack.push(n),
            Token::Op(op) => {
                let b = stack.pop().ok_or_else(|| anyhow!("missing operand"))?;
                let a = stack.pop().ok_or_else(|| anyhow!("missing operand"))?;
                let v = match op {
                    '+' => a + b,
                    '-' => a - b,
                    '*' => a * b,
                    '/' => a / b,
                    '^' => a.powf(b),
                    _ => return Err(anyhow!("bad operator")),
                };
                stack.push(v);
            }
            _ => return Err(anyhow!("malformed expression")),
        }
    }
    stack.pop().ok_or_else(|| anyhow!("empty expression"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn basic() {
        assert_eq!(eval("1 + 2").unwrap(), 3.0);
        assert_eq!(eval("(2 + 3) * 4").unwrap(), 20.0);
        assert_eq!(eval("2^10").unwrap(), 1024.0);
    }
}
