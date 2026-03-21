use anyhow::{Result, anyhow};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(f64),
    Field(String),
    BinaryOp(Box<Expr>, BinaryOp, Box<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum BinaryOp {
    Add, Sub, Mul, Div,
    Eq, Ne, Gt, Gte, Lt, Lte,
}

pub fn parse_expression(input: &str) -> Result<Expr> {
    let tokens = tokenize(input);
    let (expr, pos) = parse_expr(&tokens)?;
    if pos < tokens.len() {
        return Err(anyhow!("Unexpected tokens at end of expression: {:?}", &tokens[pos..]));
    }
    Ok(expr)
}

fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '(' | ')' | ',' => {
                if !current.is_empty() { tokens.push(current.clone()); current.clear(); }
                tokens.push(c.to_string());
                i += 1;
            }
            ' ' | '\x09' | '\x0A' => {
                if !current.is_empty() { tokens.push(current.clone()); current.clear(); }
                i += 1;
            }
            '>' => {
                if !current.is_empty() { tokens.push(current.clone()); current.clear(); }
                if i+1 < chars.len() && chars[i+1] == '=' {
                    tokens.push(">=".to_string()); i += 2;
                } else {
                    tokens.push(">".to_string()); i += 1;
                }
            }
            '<' => {
                if !current.is_empty() { tokens.push(current.clone()); current.clear(); }
                if i+1 < chars.len() && chars[i+1] == '=' {
                    tokens.push("<=".to_string()); i += 2;
                } else {
                    tokens.push("<".to_string()); i += 1;
                }
            }
            '=' => {
                if !current.is_empty() { tokens.push(current.clone()); current.clear(); }
                tokens.push("=".to_string()); i += 1;
            }
            '!' => {
                if !current.is_empty() { tokens.push(current.clone()); current.clear(); }
                if i+1 < chars.len() && chars[i+1] == '=' {
                    tokens.push("!=".to_string()); i += 2;
                } else {
                    // unexpected single !
                    current.push(c); i += 1; 
                }
            }
            '*' | '/' | '+' | '-' => {
                 if !current.is_empty() { tokens.push(current.clone()); current.clear(); }
                 tokens.push(c.to_string());
                 i += 1;
            }
            _ => {
                current.push(c);
                i += 1;
            }
        }
    }
    if !current.is_empty() { tokens.push(current); }
    tokens
}

fn parse_expr(tokens: &[String]) -> Result<(Expr, usize)> {
    parse_comparison(tokens)
}

fn parse_comparison(tokens: &[String]) -> Result<(Expr, usize)> {
    let (mut left, mut pos) = parse_term(tokens)?;
    
    while pos < tokens.len() {
        let op = match tokens[pos].as_str() {
            "=" => BinaryOp::Eq,
            "!=" => BinaryOp::Ne,
            ">" => BinaryOp::Gt,
            ">=" => BinaryOp::Gte,
            "<" => BinaryOp::Lt,
            "<=" => BinaryOp::Lte,
            _ => return Ok((left, pos)),
        };
        
        let (right, next_pos) = parse_term(&tokens[pos+1..])?;
        left = Expr::BinaryOp(Box::new(left), op, Box::new(right));
        pos += 1 + next_pos; // skip op and consumed tokens
    }
    Ok((left, pos))
}

fn parse_term(tokens: &[String]) -> Result<(Expr, usize)> {
    let (mut left, mut pos) = parse_factor(tokens)?;
    
    while pos < tokens.len() {
        let op = match tokens[pos].as_str() {
            "+" => BinaryOp::Add,
            "-" => BinaryOp::Sub,
            _ => return Ok((left, pos)),
        };
        
        let (right, next_pos) = parse_factor(&tokens[pos+1..])?;
        left = Expr::BinaryOp(Box::new(left), op, Box::new(right));
        pos += 1 + next_pos;
    }
    Ok((left, pos))
}

fn parse_factor(tokens: &[String]) -> Result<(Expr, usize)> {
    let (mut left, mut pos) = parse_primary(tokens)?;
    
    while pos < tokens.len() {
        let op = match tokens[pos].as_str() {
            "*" => BinaryOp::Mul,
            "/" => BinaryOp::Div,
            _ => return Ok((left, pos)),
        };
        
        let (right, next_pos) = parse_primary(&tokens[pos+1..])?;
        left = Expr::BinaryOp(Box::new(left), op, Box::new(right));
        pos += 1 + next_pos;
    }
    Ok((left, pos))
}

fn parse_primary(tokens: &[String]) -> Result<(Expr, usize)> {
    if tokens.is_empty() { return Err(anyhow!("Unexpected end of expression")); }
    
    let token = &tokens[0];
    if token == "(" {
        let (expr, pos) = parse_expr(&tokens[1..])?;
        if 1 + pos < tokens.len() && tokens[1+pos] == ")" {
             return Ok((expr, 1 + pos + 1));
        } else {
             return Err(anyhow!("Missing closing parenthesis"));
        }
    } else if token.eq_ignore_ascii_case("IF") {
        if tokens.len() > 1 && tokens[1] == "(" {
            let (cond, p1) = parse_expr(&tokens[2..])?;
            let mut current = 2 + p1;
            if current < tokens.len() && tokens[current] == "," {
                current += 1;
                let (true_val, p2) = parse_expr(&tokens[current..])?;
                current += p2;
                if current < tokens.len() && tokens[current] == "," {
                    current += 1;
                    let (false_val, p3) = parse_expr(&tokens[current..])?;
                    current += p3;
                    if current < tokens.len() && tokens[current] == ")" {
                        return Ok((Expr::If(Box::new(cond), Box::new(true_val), Box::new(false_val)), current + 1));
                    }
                }
            }
        }
        return Err(anyhow!("Invalid IF syntax"));
    } else if let Ok(n) = token.parse::<f64>() {
        return Ok((Expr::Literal(n), 1));
    } else {
        // Assume field
        return Ok((Expr::Field(token.clone()), 1));
    }
}

pub fn evaluate(expr: &Expr, context: &HashMap<String, f64>) -> f64 {
    match expr {
        Expr::Literal(n) => *n,
        Expr::Field(name) => *context.get(name).unwrap_or(&0.0),
        Expr::BinaryOp(left, op, right) => {
            let l = evaluate(left, context);
            let r = evaluate(right, context);
            match op {
                BinaryOp::Add => l + r,
                BinaryOp::Sub => l - r,
                BinaryOp::Mul => l * r,
                BinaryOp::Div => if r != 0.0 { l / r } else { 0.0 },
                BinaryOp::Eq => if (l - r).abs() < f64::EPSILON { 1.0 } else { 0.0 },
                BinaryOp::Ne => if (l - r).abs() > f64::EPSILON { 1.0 } else { 0.0 },
                BinaryOp::Gt => if l > r { 1.0 } else { 0.0 },
                BinaryOp::Gte => if l >= r { 1.0 } else { 0.0 },
                BinaryOp::Lt => if l < r { 1.0 } else { 0.0 },
                BinaryOp::Lte => if l <= r { 1.0 } else { 0.0 },
            }
        },
        Expr::If(cond, true_val, false_val) => {
            if evaluate(cond, context) > 0.0 {
                evaluate(true_val, context)
            } else {
                evaluate(false_val, context)
            }
        }
    }
}

pub fn extract_fields(expr: &Expr) -> Vec<String> {
    match expr {
        Expr::Literal(_) => vec![],
        Expr::Field(name) => vec![name.clone()],
        Expr::BinaryOp(left, _, right) => {
            let mut fields = extract_fields(left);
            fields.extend(extract_fields(right));
            fields
        },
        Expr::If(c, t, f) => {
            let mut fields = extract_fields(c);
            fields.extend(extract_fields(t));
            fields.extend(extract_fields(f));
            fields
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let expr = parse_expression("1 + 2 * 3").unwrap();
        assert_eq!(evaluate(&expr, &HashMap::new()), 7.0);
    }

    #[test]
    fn test_parse_if_case_insensitive() {
        let expr_str = "If(amount > 7, amount * 0.10, amount * 0.05)";
        let expr = parse_expression(expr_str).unwrap();
        
        let mut context = HashMap::new();
        context.insert("amount".to_string(), 10.0);
        assert_eq!(evaluate(&expr, &context), 1.0);
        
        context.insert("amount".to_string(), 5.0);
        assert_eq!(evaluate(&expr, &context), 0.25);
    }

    #[test]
    fn test_parse_error_trailing() {
        let res = parse_expression("1 + 2 extra");
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("Unexpected tokens"));
    }
}