use crate::mappings;
use gm8exe::GameAssets;
use gml_parser::{
    ast::{self, AST},
    token::Operator,
};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::Write,
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Mode {
    On,
    Off,
    Auto,
}

struct Deobfuscator<'a> {
    assets: &'a mut GameAssets,
    fields: Vec<Box<[u8]>>,
    constants: HashMap<&'static [u8], f64>,
    vars: HashSet<&'static str>,
}

struct ExprWriter<'de, 'src, 'dest> {
    deobf: &'de mut Deobfuscator<'src>,
    indent: usize,
    indent_str: String,
    output: &'dest mut Vec<u8>,

    is_gml_expr: bool,        // whether we're in a gml expr
    group_skip_newline: bool, // this too
}

pub fn process<'a>(assets: &'a mut GameAssets) {
    let constants = mappings::make_constants_map();
    let vars = mappings::make_kernel_vars_lut();
    let deobfuscator = Deobfuscator { assets, fields: Vec::new(), constants, vars };
}

impl<'a> Deobfuscator<'a> {
    pub fn process_gml(&mut self, input: &'a [u8]) -> Result<Vec<u8>, ast::Error> {
        let mut output = Vec::new();
        let ast = AST::new(input)?;

        let mut writer = ExprWriter {
            deobf: self,
            indent: 0,
            indent_str: "    ".into(),
            output: &mut output,

            is_gml_expr: false,
            group_skip_newline: false,
        };

        for expr in ast {
            writer.process_expr(&expr);
        }

        Ok(output)
    }

    pub fn process_expression(&mut self, input: &'a [u8]) -> Result<Vec<u8>, ast::Error> {
        let mut output = Vec::new();
        let expr = AST::expression(input)?;
        let mut writer = ExprWriter {
            deobf: self,
            indent: 0,
            indent_str: "    ".into(),
            output: &mut output,

            is_gml_expr: true,
            group_skip_newline: false,
        };
        writer.process_expr(&expr);
        Ok(output)
    }

    pub fn register_field(&mut self, field: &[u8]) -> usize {
        match self.fields.iter().position(|x| &**x == field) {
            Some(x) => x,
            None => {
                let pos = self.fields.len();
                self.fields.push(field.into());
                pos
            },
        }
    }

    pub fn simplify(&mut self, expr: &ast::Expr) -> Option<f64> {
        match expr {
            ast::Expr::LiteralIdentifier(ident) => {
                // We don't want to simplify pi.
                if ident == b"pi" {
                    None
                } else {
                    self.constants.get(ident).copied()
                }
            },
            ast::Expr::LiteralReal(real) => Some(*real),
            ast::Expr::Unary(unary) => {
                let child = self.simplify(&unary.child)?;
                match unary.op {
                    Operator::Add => Some(child),
                    Operator::Subtract => Some(-child),
                    _ => None, // technically there's others. none used by obf
                }
            },
            ast::Expr::Binary(binary) => {
                let left = self.simplify(&binary.left)?;
                let right = self.simplify(&binary.right)?;
                match binary.op {
                    Operator::Add => Some(left + right),
                    Operator::Subtract => Some(left - right),
                    _ => None,
                }
            },
            _ => None,
        }
    }
}

impl<'de, 'src, 'dest> ExprWriter<'de, 'src, 'dest> {
    pub fn process_expr(&mut self, ex: &'_ ast::Expr) {
        macro_rules! vec_extend {
            ($lit: literal) => {
                self.output.extend_from_slice(($lit).as_bytes());
            };
        }

        match ex {
            ast::Expr::LiteralIdentifier(expr) => (),
            ast::Expr::LiteralReal(real) => {
                let _ = write!(self.output, "{}", real);
            },
            ast::Expr::LiteralString(string) => {
                let quote = if string.iter().any(|&x| x == b'"') { b'\'' } else { b'"' };
                self.output.push(quote);
                self.output.extend_from_slice(string);
                self.output.push(quote);
            },
            ast::Expr::Unary(expr) => {
                let op = op_to_str(expr.op);
                self.output.extend_from_slice(op.as_bytes());
                let prev_state = self.is_gml_expr;
                self.is_gml_expr = true;
                let is_child_binary = matches!(expr.child, ast::Expr::Binary(_));
                if is_child_binary {
                    vec_extend!("(");
                }
                self.process_expr(&expr.child);
                if is_child_binary {
                    vec_extend!(")");
                }
                self.is_gml_expr = prev_state;
            },
            ast::Expr::Binary(expr) => {
                let prev_state = self.is_gml_expr;
                self.is_gml_expr = true;
                if let Some(simple) = self.deobf.simplify(ex) {
                    self.process_expr(&ast::Expr::LiteralReal(simple));
                } else {
                    if expr.op == Operator::Index {
                        // array indexing
                        self.process_expr(&expr.left);
                        self.output.push(b'[');
                        if let ast::Expr::Group(group) = &expr.right {
                            for (i, expr) in group.iter().enumerate() {
                                if i != 0 { vec_extend!(", "); }
                                self.process_expr(expr);
                            }
                        } else {
                            panic!("index rhs wasn't a group");
                        }
                        self.output.push(b']');
                    } else if expr.op == Operator::Deref {
                        // TODO: Unfuck object id literals
                        let simple = self.deobf.simplify(&expr.left);
                        if let Some(obj_id) = simple.and_then(|x| {
                            self.deobf.assets.objects.iter()
                                .enumerate()
                                .filter_map(|(i, o)| o.as_ref().map(|v| (i, v)))
                                .find(|(i, _)| (*i as f64 - x).abs() <= 1e-14)
                                .map(|(i, _)| i)
                        }) {
                            // i think the epsilon thing is right?       ^
                            // process that here yeah
                        }
                    }
                }
                self.is_gml_expr = prev_state;
                if !self.is_gml_expr {
                    vec_extend!(";\r\n");
                }
            },
            ast::Expr::DoUntil(expr) => {
                vec_extend!("do ");
                self.write_expr_grouped(&expr.body, false);
                vec_extend!("until (");
                self.is_gml_expr = true;
                self.process_expr(&expr.cond);
                self.is_gml_expr = false;
                vec_extend!(");\r\n");
            },
            ast::Expr::For(expr) => {
                fn remove_truncate(x: &mut Vec<u8>, pat: &[u8]) {
                    if x.ends_with(pat) {
                        x.truncate(x.len() - pat.len());
                    }
                }

                vec_extend!("for (");
                self.is_gml_expr = true;
                self.process_expr(&expr.start);
                remove_truncate(&mut self.output, b"\r\n");
                self.process_expr(&expr.cond);
                vec_extend!("; ");
                self.process_expr(&expr.step);
                remove_truncate(&mut self.output, b"\r\n");
                remove_truncate(&mut self.output, b";");
                vec_extend!(") ");
                self.is_gml_expr = false;
                self.write_expr_grouped(&expr.body, true);
            },
            ast::Expr::Function(expr) => {
                if let Some(idx) = self
                    .deobf
                    .assets
                    .scripts
                    .iter()
                    .enumerate()
                    .filter_map(|(i, o)| o.as_ref().map(|x| (i, x)))
                    .find(|(_, scr)| &*scr.name.0 == expr.name)
                    .map(|(i, _)| i)
                {
                    let _ = write!(self.output, "script{}", idx);
                } else {
                    self.output.extend_from_slice(expr.name);
                }
                self.output.push(b'(');
                let prev_expr_state = self.is_gml_expr;
                self.is_gml_expr = true;
                for (i, param) in expr.params.iter().enumerate() {
                    if i != 0 {
                        vec_extend!(", ");
                    }
                    self.process_expr(param);
                }
                self.is_gml_expr = prev_expr_state;
                if self.is_gml_expr {
                    vec_extend!(")");
                } else {
                    vec_extend!(");\r\n");
                }
            },
            ast::Expr::Group(exprs) => {
                vec_extend!("{\r\n");
                self.indent += 1;
                let mut is_case = false;
                for expr in exprs {
                    if matches!(expr, ast::Expr::Case(_) | ast::Expr::Default) {
                        if is_case {
                            self.indent -= 1;
                        } else {
                            self.indent += 1;
                            is_case = true;
                        }
                        self.write_indent();
                        self.process_expr(expr);
                        self.indent += 1;
                    } else {
                        self.write_indent();
                        self.process_expr(expr);
                    }
                }
                if is_case {
                    self.indent -= 2;
                }
                self.indent -= 1;
                if self.group_skip_newline {
                    self.output.push(b'}');
                    self.group_skip_newline = false;
                } else {
                    vec_extend!("}\r\n");
                }
            },
            ast::Expr::If(expr) => {
                vec_extend!("if (");
                self.is_gml_expr = true;
                self.process_expr(&expr.cond);
                self.is_gml_expr = false;
                vec_extend!(") ");

                self.write_expr_grouped(&expr.body, false);

                if let Some(expr_else) = &expr.else_body {
                    vec_extend!(" else ");
                    self.process_expr(expr_else);
                } else {
                    vec_extend!("\r\n");
                }
            },
            ast::Expr::Repeat(expr) => {
                vec_extend!("repeat (");
                self.is_gml_expr = true;
                self.process_expr(&expr.count);
                self.is_gml_expr = false;
                vec_extend!(") ");
                self.write_expr_grouped(&expr.body, true);
            },
            ast::Expr::Switch(expr) => {
                vec_extend!("switch (");
                self.is_gml_expr = true;
                self.process_expr(&expr.input);
                self.is_gml_expr = false;
                vec_extend!(") ");
                self.write_expr_grouped(&expr.body, true);
            },
            ast::Expr::Var(expr) => {
                if expr.vars.len() > 0 {
                    vec_extend!("var ");
                    for (i, name) in expr.vars.iter().enumerate() {
                        if i != 0 {
                            vec_extend!(", ");
                        }
                        self.write_field(name);
                    }
                    vec_extend!(";\r\n");
                }
            },
            ast::Expr::GlobalVar(expr) => {
                if expr.vars.len() > 0 {
                    vec_extend!("globalvar ");
                    for (i, name) in expr.vars.iter().enumerate() {
                        if i != 0 {
                            vec_extend!(", ");
                        }
                        self.write_field(name);
                    }
                    vec_extend!(";\r\n");
                }
            },
            ast::Expr::With(expr) => {
                vec_extend!("with (");
                // TODO: Deobfuscate target name / value
                self.is_gml_expr = true;
                self.process_expr(&expr.target);
                self.is_gml_expr = false;
                vec_extend!(") ");
                self.write_expr_grouped(&expr.body, true);
            },
            ast::Expr::While(expr) => {
                vec_extend!("while (");
                self.is_gml_expr = true;
                self.process_expr(&expr.cond);
                self.is_gml_expr = false;
                vec_extend!(") ");
                self.write_expr_grouped(&expr.body, true);
            },
            ast::Expr::Case(expr) => {
                vec_extend!("case ");
                self.is_gml_expr = true;
                self.process_expr(expr);
                self.is_gml_expr = false;
                vec_extend!(":\r\n");
            },
            ast::Expr::Default => vec_extend!("default:\r\n"),
            ast::Expr::Continue => vec_extend!("continue;\r\n"),
            ast::Expr::Break => vec_extend!("break;\r\n"),
            ast::Expr::Exit => vec_extend!("exit;\r\n"),
            ast::Expr::Return(expr) => {
                vec_extend!("return ");
                self.is_gml_expr = true;
                self.process_expr(expr);
                self.is_gml_expr = false;
                vec_extend!(";\r\n");
            },
        }
    }

    pub fn write_expr_grouped(&mut self, expr: &ast::Expr, newline: bool) {
        if matches!(expr, ast::Expr::Group(_)) {
            if !newline {
                self.group_skip_newline = true;
            }
            self.process_expr(expr);
        } else {
            self.output.extend_from_slice(b"{\r\n");
            self.indent += 1;
            self.write_indent();
            self.process_expr(expr);
            self.indent -= 1;
            self.write_indent();
            self.output.extend_from_slice(if newline { b"}\r\n" } else { b"}" });
        }
    }

    pub fn write_field(&mut self, ident: &[u8]) {
        let field_number = self.deobf.register_field(ident);
        let _ = write!(self.output, "field{}", field_number);
    }

    pub fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.output.extend_from_slice(self.indent_str.as_bytes());
        }
    }
}

fn op_to_str(op: Operator) -> &'static str {
    match op {
        Operator::Add => "+",
        Operator::Subtract => "-",
        Operator::Multiply => "*",
        Operator::Divide => "/",
        Operator::IntDivide => "div",
        Operator::BitwiseAnd => "&",
        Operator::BitwiseOr => "|",
        Operator::BitwiseXor => "^",
        Operator::Assign => "=",
        Operator::Not => "!",
        Operator::LessThan => "<",
        Operator::GreaterThan => ">",
        Operator::AssignAdd => "+=",
        Operator::AssignSubtract => "-=",
        Operator::AssignMultiply => "*=",
        Operator::AssignDivide => "/=",
        Operator::AssignBitwiseAnd => "&=",
        Operator::AssignBitwiseOr => "|=",
        Operator::AssignBitwiseXor => "^=",
        Operator::Equal => "==",
        Operator::NotEqual => "!=",
        Operator::LessThanOrEqual => "<=",
        Operator::GreaterThanOrEqual => ">=",
        Operator::Modulo => "mod",
        Operator::And => "&&",
        Operator::Or => "||",
        Operator::Xor => "^^",
        Operator::BinaryShiftLeft => "<<",
        Operator::BinaryShiftRight => ">>",
        Operator::Complement => "~",
        Operator::Deref => ".",
        Operator::Index => panic!("index op passed to op_to_str"),
    }
}