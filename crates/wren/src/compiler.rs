//! Source to bytecode, in one pass.
//!
//! A Pratt parser, as upstream's `wren_compiler.c` is: each token kind has a
//! prefix rule, an infix rule and a precedence, and parsing an expression means
//! running the prefix rule and then consuming infix rules while they bind
//! tightly enough. It fits the language well because Wren's operators are all
//! methods, so the infix rules mostly do the same thing with a different
//! signature.
//!
//! **One pass, straight to bytecode — no syntax tree.** That is upstream's
//! design and it is the right one here for a reason beyond speed: a tree of
//! nodes for a program is a large allocation on a part measured in kilobytes,
//! and it exists only to be walked once.
//!
//! # What this compiles
//!
//! Expressions, variables, `if`/`else`, `while`, `for`-`in`, blocks, list
//! literals and string interpolation. **Not** functions, classes, fibers or
//! imports — those need call frames in the VM, which do not exist yet.

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::bytecode::{Chunk, Op};
use crate::core;
use crate::lexer::{Lexer, Token, TokenKind};
use crate::value::Value;
use crate::vm::Vm;

/// A failure to compile, with the line it was found on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    pub message: String,
    pub line: u16,
}

/// How tightly an operator binds. Upstream's order, and its names.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Precedence {
    None,
    Lowest,
    Assignment, // =
    LogicalOr,  // ||
    LogicalAnd, // &&
    Equality,   // == !=
    Comparison, // < > <= >=
    Range,      // .. ...
    Term,       // + -
    Factor,     // * / %
    Unary,      // - !
    Call,       // . ( [
}

/// A local variable: its name, and the scope it belongs to.
struct Local {
    name: String,
    depth: i32,
}

/// Compile `source` into a chunk that [`Vm::run`](crate::vm::Vm::run) can
/// execute.
pub fn compile(vm: &mut Vm, source: &str) -> Result<Chunk, CompileError> {
    let mut compiler = Compiler::new(vm, source);
    compiler.advance()?;
    compiler.skip_newlines()?;

    while compiler.current.kind != TokenKind::Eof {
        compiler.declaration()?;
        compiler.skip_newlines()?;
    }

    let line = clamp_line(compiler.current.line);
    compiler.chunk.emit_op(Op::End, line);
    Ok(compiler.chunk)
}

struct Compiler<'a> {
    vm: &'a mut Vm,
    source: &'a str,
    lexer: Lexer<'a>,
    previous: Token,
    current: Token,
    chunk: Chunk,
    locals: Vec<Local>,
    /// `-1` means module level, where a `var` becomes a module variable rather
    /// than a stack slot. Upstream uses the same sentinel for the same reason.
    scope_depth: i32,
}

impl<'a> Compiler<'a> {
    fn new(vm: &'a mut Vm, source: &'a str) -> Compiler<'a> {
        let placeholder = Token { kind: TokenKind::Eof, start: 0, end: 0, line: 1 };
        Compiler {
            vm,
            source,
            lexer: Lexer::new(source),
            previous: placeholder,
            current: placeholder,
            chunk: Chunk::new(),
            locals: Vec::new(),
            scope_depth: -1,
        }
    }

    // --- tokens -------------------------------------------------------------

    fn advance(&mut self) -> Result<(), CompileError> {
        self.previous = self.current;
        self.current = self.lexer.next_token();
        if self.current.kind == TokenKind::Error {
            return Err(self.error_at(self.current, "Invalid token."));
        }
        Ok(())
    }

    fn check(&self, kind: TokenKind) -> bool {
        self.current.kind == kind
    }

    fn match_token(&mut self, kind: TokenKind) -> Result<bool, CompileError> {
        if self.check(kind) {
            self.advance()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn consume(&mut self, kind: TokenKind, message: &str) -> Result<(), CompileError> {
        if self.check(kind) {
            return self.advance();
        }
        Err(self.error_at(self.current, message))
    }

    /// Consume any run of newlines.
    ///
    /// **Newlines are statement separators in Wren**, so they cannot simply be
    /// skipped by the lexer — but there are many places where one is
    /// meaningless, such as after an operator or inside a bracket. Those places
    /// call this, which is exactly what upstream's `ignoreNewlines` does.
    fn skip_newlines(&mut self) -> Result<(), CompileError> {
        while self.check(TokenKind::Line) {
            self.advance()?;
        }
        Ok(())
    }

    /// Require the end of a statement.
    fn consume_line(&mut self, message: &str) -> Result<(), CompileError> {
        if self.check(TokenKind::Eof) || self.check(TokenKind::RightBrace) {
            return Ok(());
        }
        self.consume(TokenKind::Line, message)?;
        self.skip_newlines()
    }

    fn error_at(&self, token: Token, message: &str) -> CompileError {
        CompileError { message: message.to_string(), line: clamp_line(token.line) }
    }

    fn line(&self) -> u16 {
        clamp_line(self.previous.line)
    }

    // --- scopes and variables -----------------------------------------------

    fn begin_scope(&mut self) {
        self.scope_depth += 1;
    }

    /// Close a scope, discarding the locals it held.
    ///
    /// Each one is a live stack slot, so leaving it would corrupt every slot
    /// index above it — this is where the compiler's idea of the stack and the
    /// VM's have to agree exactly.
    fn end_scope(&mut self) {
        let line = self.line();
        while let Some(local) = self.locals.last() {
            if local.depth < self.scope_depth {
                break;
            }
            self.chunk.emit_op(Op::Pop, line);
            self.locals.pop();
        }
        self.scope_depth -= 1;
    }

    fn resolve_local(&self, name: &str) -> Option<usize> {
        // Backwards, so an inner scope's variable shadows an outer one.
        self.locals.iter().rposition(|local| local.name == name)
    }

    /// Declare a local occupying the slot the value on top of the stack is in.
    ///
    /// The invariant this depends on: **when a local is declared, the stack
    /// height equals the number of locals already declared.** Every statement
    /// leaves the stack as it found it, which is what keeps that true.
    fn add_local(&mut self, name: &str) -> Result<usize, CompileError> {
        if self.locals.len() >= u8::MAX as usize {
            return Err(self.error_at(self.previous, "Too many local variables in scope."));
        }
        self.locals.push(Local { name: name.to_string(), depth: self.scope_depth });
        Ok(self.locals.len() - 1)
    }

    // --- declarations and statements ----------------------------------------

    fn declaration(&mut self) -> Result<(), CompileError> {
        if self.match_token(TokenKind::Var)? {
            self.var_declaration()?;
        } else {
            self.statement()?;
        }
        // **The terminator is consumed here and nowhere else.** A statement
        // that consumed its own newline could not appear as the then-branch of
        // a one-line `if`, because `else` would come where the newline was
        // expected. Upstream splits it the same way for the same reason.
        self.consume_line("Expect newline after statement.")
    }

    fn var_declaration(&mut self) -> Result<(), CompileError> {
        self.consume(TokenKind::Name, "Expect variable name.")?;
        let name = self.previous.text(self.source).to_string();
        let line = self.line();

        if self.match_token(TokenKind::Eq)? {
            self.skip_newlines()?;
            self.expression()?;
        } else {
            self.chunk.emit_op(Op::Null, line);
        }

        if self.scope_depth < 0 {
            // Module level: the variable lives in the module, not on the stack.
            let index = self.vm.module.define(&name, Value::NULL);
            self.chunk.emit_op(Op::StoreModuleVar, line);
            self.chunk.emit_short(index as u16, line);
            self.chunk.emit_op(Op::Pop, line);
        } else {
            // The value is already in the right slot; the local just names it.
            self.add_local(&name)?;
        }
        Ok(())
    }

    fn statement(&mut self) -> Result<(), CompileError> {
        if self.match_token(TokenKind::LeftBrace)? {
            self.begin_scope();
            self.block()?;
            self.end_scope();
            return Ok(());
        }
        if self.match_token(TokenKind::If)? {
            return self.if_statement();
        }
        if self.match_token(TokenKind::While)? {
            return self.while_statement();
        }
        if self.match_token(TokenKind::For)? {
            return self.for_statement();
        }
        self.expression_statement()
    }

    fn block(&mut self) -> Result<(), CompileError> {
        self.skip_newlines()?;
        while !self.check(TokenKind::RightBrace) && !self.check(TokenKind::Eof) {
            self.declaration()?;
            self.skip_newlines()?;
        }
        self.consume(TokenKind::RightBrace, "Expect '}' after block.")
    }

    fn if_statement(&mut self) -> Result<(), CompileError> {
        self.consume(TokenKind::LeftParen, "Expect '(' after 'if'.")?;
        self.skip_newlines()?;
        self.expression()?;
        self.skip_newlines()?;
        self.consume(TokenKind::RightParen, "Expect ')' after if condition.")?;

        let line = self.line();
        let else_jump = self.chunk.emit_jump(Op::JumpIf, line);
        self.skip_newlines()?;
        self.statement()?;

        if self.match_token(TokenKind::Else)? {
            let line = self.line();
            let end_jump = self.chunk.emit_jump(Op::Jump, line);
            self.patch(else_jump)?;
            self.skip_newlines()?;
            self.statement()?;
            self.patch(end_jump)?;
        } else {
            self.patch(else_jump)?;
        }
        Ok(())
    }

    fn while_statement(&mut self) -> Result<(), CompileError> {
        let loop_start = self.chunk.code.len();
        self.consume(TokenKind::LeftParen, "Expect '(' after 'while'.")?;
        self.skip_newlines()?;
        self.expression()?;
        self.skip_newlines()?;
        self.consume(TokenKind::RightParen, "Expect ')' after while condition.")?;

        let line = self.line();
        let exit = self.chunk.emit_jump(Op::JumpIf, line);
        self.skip_newlines()?;
        self.statement()?;

        let line = self.line();
        if !self.chunk.emit_loop(loop_start, line) {
            return Err(self.error_at(self.previous, "Loop body too large."));
        }
        self.patch(exit)
    }

    /// `for (name in sequence) body`
    ///
    /// Desugared to the iteration protocol, exactly as upstream does it:
    ///
    /// ```text
    /// {
    ///   var seq_  = sequence
    ///   var iter_ = null
    ///   while (iter_ = seq_.iterate(iter_)) {
    ///     var name = seq_.iteratorValue(iter_)
    ///     body
    ///   }
    /// }
    /// ```
    ///
    /// The two hidden locals are named with a trailing space so that no Wren
    /// program can refer to them — upstream's trick, and it costs nothing.
    fn for_statement(&mut self) -> Result<(), CompileError> {
        self.begin_scope();

        self.consume(TokenKind::LeftParen, "Expect '(' after 'for'.")?;
        self.consume(TokenKind::Name, "Expect for loop variable name.")?;
        let variable = self.previous.text(self.source).to_string();
        self.consume(TokenKind::In, "Expect 'in' after loop variable.")?;
        self.skip_newlines()?;

        self.expression()?;
        let sequence_slot = self.add_local("seq ")?;

        let line = self.line();
        self.chunk.emit_op(Op::Null, line);
        let iterator_slot = self.add_local("iter ")?;

        self.consume(TokenKind::RightParen, "Expect ')' after loop expression.")?;

        let loop_start = self.chunk.code.len();

        // iter_ = seq_.iterate(iter_)
        let line = self.line();
        self.emit_load_local(sequence_slot, line);
        self.emit_load_local(iterator_slot, line);
        self.emit_call("iterate(_)", 1, line)?;
        self.chunk.emit_op(Op::StoreLocal, line);
        self.chunk.emit_byte(iterator_slot as u8, line);

        // `StoreLocal` leaves the value, and `JumpIf` consumes it -- so the
        // assignment is also the loop condition, with nothing extra emitted.
        let exit = self.chunk.emit_jump(Op::JumpIf, line);

        // The loop variable is a fresh local in a scope of its own, so that a
        // closure capturing it would get this iteration's value. (Closures do
        // not exist yet; the scope is still the right shape.)
        self.begin_scope();
        self.emit_load_local(sequence_slot, line);
        self.emit_load_local(iterator_slot, line);
        self.emit_call("iteratorValue(_)", 1, line)?;
        self.add_local(&variable)?;

        self.skip_newlines()?;
        self.statement()?;
        self.end_scope();

        let line = self.line();
        if !self.chunk.emit_loop(loop_start, line) {
            return Err(self.error_at(self.previous, "Loop body too large."));
        }
        self.patch(exit)?;

        self.end_scope();
        Ok(())
    }

    fn expression_statement(&mut self) -> Result<(), CompileError> {
        self.expression()?;
        let line = self.line();
        self.chunk.emit_op(Op::Pop, line);
        Ok(())
    }

    // --- expressions --------------------------------------------------------

    fn expression(&mut self) -> Result<(), CompileError> {
        self.parse_precedence(Precedence::Lowest)
    }

    fn parse_precedence(&mut self, precedence: Precedence) -> Result<(), CompileError> {
        self.advance()?;
        let can_assign = precedence <= Precedence::Assignment;
        self.prefix(can_assign)?;

        while precedence <= infix_precedence(self.current.kind) {
            self.advance()?;
            self.infix(can_assign)?;
        }

        if can_assign && self.check(TokenKind::Eq) {
            return Err(self.error_at(self.current, "Invalid assignment target."));
        }
        Ok(())
    }

    fn prefix(&mut self, can_assign: bool) -> Result<(), CompileError> {
        let token = self.previous;
        let line = clamp_line(token.line);
        match token.kind {
            TokenKind::Number => {
                let value = token
                    .number(self.source)
                    .ok_or_else(|| self.error_at(token, "Invalid number literal."))?;
                self.emit_constant(Value::num(value), line);
                Ok(())
            }
            TokenKind::String => {
                let text = unescape(token.text(self.source));
                let value = self.vm.new_string(&text);
                self.emit_constant(value, line);
                Ok(())
            }
            TokenKind::Interpolation => self.interpolation(),
            TokenKind::True => {
                self.chunk.emit_op(Op::True, line);
                Ok(())
            }
            TokenKind::False => {
                self.chunk.emit_op(Op::False, line);
                Ok(())
            }
            TokenKind::Null => {
                self.chunk.emit_op(Op::Null, line);
                Ok(())
            }
            TokenKind::Name => self.variable(can_assign),
            TokenKind::LeftParen => {
                self.skip_newlines()?;
                self.expression()?;
                self.skip_newlines()?;
                self.consume(TokenKind::RightParen, "Expect ')' after expression.")
            }
            TokenKind::LeftBracket => self.list_literal(),
            TokenKind::Minus => {
                self.parse_precedence(Precedence::Unary)?;
                self.emit_call("-", 0, line)
            }
            TokenKind::Bang => {
                self.parse_precedence(Precedence::Unary)?;
                self.emit_call("!", 0, line)
            }
            _ => Err(self.error_at(token, "Expected expression.")),
        }
    }

    fn infix(&mut self, _can_assign: bool) -> Result<(), CompileError> {
        let token = self.previous;
        let line = clamp_line(token.line);

        // `&&` and `||` are the only operators that are not method calls: they
        // have to be able to *not* evaluate their right side.
        match token.kind {
            TokenKind::AmpAmp => {
                let jump = self.chunk.emit_jump(Op::And, line);
                self.skip_newlines()?;
                self.parse_precedence(Precedence::LogicalAnd)?;
                return self.patch(jump);
            }
            TokenKind::PipePipe => {
                let jump = self.chunk.emit_jump(Op::Or, line);
                self.skip_newlines()?;
                self.parse_precedence(Precedence::LogicalOr)?;
                return self.patch(jump);
            }
            TokenKind::Dot => return self.method_call(),
            TokenKind::LeftBracket => return self.subscript(),
            _ => {}
        }

        let name = match token.kind {
            TokenKind::Plus => "+",
            TokenKind::Minus => "-",
            TokenKind::Star => "*",
            TokenKind::Slash => "/",
            TokenKind::Percent => "%",
            TokenKind::Lt => "<",
            TokenKind::Gt => ">",
            TokenKind::LtEq => "<=",
            TokenKind::GtEq => ">=",
            TokenKind::EqEq => "==",
            TokenKind::BangEq => "!=",
            TokenKind::DotDot => "..",
            TokenKind::DotDotDot => "...",
            _ => return Err(self.error_at(token, "Expected operator.")),
        };

        // Right side binds one level tighter, which is what makes these
        // operators left-associative.
        let next = match infix_precedence(token.kind) {
            Precedence::Call => Precedence::Call,
            other => tighter(other),
        };
        self.skip_newlines()?;
        self.parse_precedence(next)?;
        self.emit_call(&signature(name, 1), 1, line)
    }

    /// `receiver.name`, `receiver.name(a, b)` or `receiver.name = value`.
    fn method_call(&mut self) -> Result<(), CompileError> {
        self.consume(TokenKind::Name, "Expect method name after '.'.")?;
        let name = self.previous.text(self.source).to_string();
        let line = self.line();

        if self.match_token(TokenKind::LeftParen)? {
            let arity = self.argument_list()?;
            return self.emit_call(&signature(&name, arity), arity, line);
        }

        if self.check(TokenKind::Eq) {
            self.advance()?;
            self.skip_newlines()?;
            self.expression()?;
            return self.emit_call(&format!("{name}=(_)"), 1, line);
        }

        // No parentheses and no assignment: a getter.
        self.emit_call(&name, 0, line)
    }

    fn subscript(&mut self) -> Result<(), CompileError> {
        let line = self.line();
        self.skip_newlines()?;
        self.expression()?;
        self.skip_newlines()?;
        self.consume(TokenKind::RightBracket, "Expect ']' after subscript.")?;

        if self.check(TokenKind::Eq) {
            self.advance()?;
            self.skip_newlines()?;
            self.expression()?;
            return self.emit_call("[_]=(_)", 2, line);
        }
        self.emit_call("[_]", 1, line)
    }

    fn argument_list(&mut self) -> Result<usize, CompileError> {
        let mut arity = 0;
        self.skip_newlines()?;
        if !self.check(TokenKind::RightParen) {
            loop {
                self.skip_newlines()?;
                self.expression()?;
                arity += 1;
                if arity > 16 {
                    return Err(self.error_at(self.current, "Cannot pass more than 16 arguments."));
                }
                self.skip_newlines()?;
                if !self.match_token(TokenKind::Comma)? {
                    break;
                }
            }
        }
        self.skip_newlines()?;
        self.consume(TokenKind::RightParen, "Expect ')' after arguments.")?;
        Ok(arity)
    }

    fn variable(&mut self, can_assign: bool) -> Result<(), CompileError> {
        let name = self.previous.text(self.source).to_string();
        let line = self.line();

        if can_assign && self.check(TokenKind::Eq) {
            self.advance()?;
            self.skip_newlines()?;
            self.expression()?;

            if let Some(slot) = self.resolve_local(&name) {
                self.chunk.emit_op(Op::StoreLocal, line);
                self.chunk.emit_byte(slot as u8, line);
                return Ok(());
            }
            let Some(index) = self.vm.module.names.find(&name) else {
                return Err(self.error_at(self.previous, "Variable is not defined."));
            };
            self.chunk.emit_op(Op::StoreModuleVar, line);
            self.chunk.emit_short(index as u16, line);
            return Ok(());
        }

        if let Some(slot) = self.resolve_local(&name) {
            self.emit_load_local(slot, line);
            return Ok(());
        }
        let Some(index) = self.vm.module.names.find(&name) else {
            return Err(self.error_at(self.previous, "Variable is not defined."));
        };
        self.chunk.emit_op(Op::LoadModuleVar, line);
        self.chunk.emit_short(index as u16, line);
        Ok(())
    }

    /// `[a, b, c]`
    ///
    /// Built by calling `List.new` and then `addCore(_)` per element, which is
    /// upstream's approach. It needs no opcode of its own, and `addCore`
    /// returns the list so it stays on the stack between elements.
    fn list_literal(&mut self) -> Result<(), CompileError> {
        let line = self.line();
        let Some(index) = self.vm.module.names.find("List") else {
            return Err(self.error_at(self.previous, "List class is not defined."));
        };
        self.chunk.emit_op(Op::LoadModuleVar, line);
        self.chunk.emit_short(index as u16, line);
        self.emit_call("new", 0, line)?;

        self.skip_newlines()?;
        if !self.check(TokenKind::RightBracket) {
            loop {
                self.skip_newlines()?;
                if self.check(TokenKind::RightBracket) {
                    break;
                }
                self.expression()?;
                self.emit_call("addCore(_)", 1, line)?;
                self.skip_newlines()?;
                if !self.match_token(TokenKind::Comma)? {
                    break;
                }
            }
        }
        self.skip_newlines()?;
        self.consume(TokenKind::RightBracket, "Expect ']' after list elements.")
    }

    /// `"a%(b)c"`
    ///
    /// Compiled as `"a" + b.toString + "c"`. Upstream builds a list and calls
    /// `join` on it; concatenation is the smaller thing to emit and produces
    /// the same string, at the cost of an intermediate per interpolation.
    fn interpolation(&mut self) -> Result<(), CompileError> {
        let line = clamp_line(self.previous.line);
        let head = unescape(self.previous.text(self.source));
        let value = self.vm.new_string(&head);
        self.emit_constant(value, line);

        loop {
            self.skip_newlines()?;
            self.expression()?;
            self.emit_call("toString", 0, line)?;
            self.emit_call("+(_)", 1, line)?;

            // **No `)` to consume.** The lexer counts parentheses inside an
            // interpolation itself, and the one that closes it is swallowed
            // there -- what arrives next is already the resumed string: another
            // `Interpolation` if there is a further `%(`, or the final `String`.
            if self.match_token(TokenKind::Interpolation)? {
                let text = unescape(self.previous.text(self.source));
                let value = self.vm.new_string(&text);
                self.emit_constant(value, line);
                self.emit_call("+(_)", 1, line)?;
                continue;
            }

            self.consume(TokenKind::String, "Expect end of string interpolation.")?;
            let text = unescape(self.previous.text(self.source));
            let value = self.vm.new_string(&text);
            self.emit_constant(value, line);
            self.emit_call("+(_)", 1, line)?;
            return Ok(());
        }
    }

    // --- emitting -----------------------------------------------------------

    fn emit_constant(&mut self, value: Value, line: u16) {
        let index = self.chunk.add_constant(value);
        self.chunk.emit_op(Op::Constant, line);
        self.chunk.emit_short(index, line);
    }

    fn emit_load_local(&mut self, slot: usize, line: u16) {
        self.chunk.emit_op(Op::LoadLocal, line);
        self.chunk.emit_byte(slot as u8, line);
    }

    fn emit_call(&mut self, signature: &str, arity: usize, line: u16) -> Result<(), CompileError> {
        let symbol = self.vm.method_names.ensure(signature);
        if symbol > u16::MAX as usize {
            return Err(self.error_at(self.previous, "Too many method names."));
        }
        self.chunk.emit_op(Op::Call, line);
        self.chunk.emit_byte(arity as u8, line);
        self.chunk.emit_short(symbol as u16, line);
        Ok(())
    }

    fn patch(&mut self, at: usize) -> Result<(), CompileError> {
        if self.chunk.patch_jump(at) {
            return Ok(());
        }
        Err(self.error_at(self.previous, "Too much code to jump over."))
    }
}

/// The signature a call compiles to: `name`, `name(_)`, `name(_,_)`.
///
/// **A Wren method is identified by name *and* arity**, so `foo(1)` and
/// `foo(1, 2)` are different methods and neither is an overload of the other.
/// Building the signature here is what makes that true in the bytecode.
fn signature(name: &str, arity: usize) -> String {
    if arity == 0 {
        return name.to_string();
    }
    let mut out = String::from(name);
    out.push('(');
    for index in 0..arity {
        if index > 0 {
            out.push(',');
        }
        out.push('_');
    }
    out.push(')');
    out
}

fn infix_precedence(kind: TokenKind) -> Precedence {
    match kind {
        TokenKind::PipePipe => Precedence::LogicalOr,
        TokenKind::AmpAmp => Precedence::LogicalAnd,
        TokenKind::EqEq | TokenKind::BangEq => Precedence::Equality,
        TokenKind::Lt | TokenKind::Gt | TokenKind::LtEq | TokenKind::GtEq => Precedence::Comparison,
        TokenKind::DotDot | TokenKind::DotDotDot => Precedence::Range,
        TokenKind::Plus | TokenKind::Minus => Precedence::Term,
        TokenKind::Star | TokenKind::Slash | TokenKind::Percent => Precedence::Factor,
        TokenKind::Dot | TokenKind::LeftBracket => Precedence::Call,
        _ => Precedence::None,
    }
}

/// The next precedence up, for left-associative operators.
fn tighter(precedence: Precedence) -> Precedence {
    match precedence {
        Precedence::None => Precedence::Lowest,
        Precedence::Lowest => Precedence::Assignment,
        Precedence::Assignment => Precedence::LogicalOr,
        Precedence::LogicalOr => Precedence::LogicalAnd,
        Precedence::LogicalAnd => Precedence::Equality,
        Precedence::Equality => Precedence::Comparison,
        Precedence::Comparison => Precedence::Range,
        Precedence::Range => Precedence::Term,
        Precedence::Term => Precedence::Factor,
        Precedence::Factor => Precedence::Unary,
        Precedence::Unary => Precedence::Call,
        Precedence::Call => Precedence::Call,
    }
}

/// Decode the escapes in a string literal.
///
/// The lexer deliberately leaves them alone — it has no allocator and a token
/// is a span, so it cannot produce a decoded string. This is where that is
/// paid for, and it is the only place that knows the escape set.
fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut characters = raw.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('%') => out.push('%'),
            Some('a') => out.push('\u{7}'),
            Some('b') => out.push('\u{8}'),
            Some('e') => out.push('\u{1b}'),
            Some('f') => out.push('\u{c}'),
            Some('v') => out.push('\u{b}'),
            // An unknown escape keeps both characters rather than swallowing
            // one, so a mistake in a program is visible in its output.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Elements for a list literal, used by [`core`] when it builds one directly.
pub fn empty_list(vm: &mut Vm) -> Value {
    core::new_list(vm, Vec::new())
}

/// Narrow a lexer line number to what a chunk stores.
///
/// The chunk keeps a line per *byte* of code, so the difference between `u16`
/// and `u32` here is two bytes for every byte of instruction — which on these
/// parts is the larger consideration. A source file past 65,535 lines reports
/// its last errors against that line rather than growing every chunk to
/// accommodate a file nobody is going to put on a microcontroller.
fn clamp_line(line: u32) -> u16 {
    line.min(u16::MAX as u32) as u16
}
