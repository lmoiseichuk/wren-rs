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
//! What single-pass costs, so the trade is on the record rather than implied:
//!
//! * **No constant folding, no dead-code elimination, no peephole pass.**
//!   `1 + 2` compiles to two constants and a call, every time. A tree would
//!   make those easy; without one they would have to be done on the byte
//!   stream after the fact, which is step 6's territory rather than this one's.
//! * **Jump offsets have to be patched.** The compiler emits a placeholder and
//!   fills it in once the destination is known, which is why `patch_jump`
//!   returns a `bool` and every caller has to handle a body too long to jump
//!   over. A tree would know the size before emitting anything.
//! * **Errors are reported at the first failure.** There is no recovery and no
//!   second error, because there is no tree to resynchronise against.
//!
//! What it buys is that a program's peak compile-time memory is its bytecode
//! plus one token of lookahead, which on a part with 8 KB is the difference
//! between compiling on the device and not.
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

use alloc::boxed::Box;
use alloc::rc::Rc;

use crate::bytecode::{Chunk, Op};
use crate::object::{ObjFn, Object};
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
    Assignment,  // =
    Conditional, // ?:
    LogicalOr,   // ||
    LogicalAnd, // &&
    Equality,     // == !=
    Is,           // is
    Comparison,   // < > <= >=
    BitwiseOr,    // |
    BitwiseXor,   // ^
    BitwiseAnd,   // &
    BitwiseShift, // << >>
    Range,        // .. ...
    Term,       // + -
    Factor,     // * / %
    Unary,      // - !
    Call,       // . ( [
}

/// How many locals one function may have, matching upstream's `MAX_LOCALS`.
///
/// Set by the bytecode rather than by taste: `LoadLocal` takes a `u8`, so slot
/// 255 is the last one addressable.
const MAX_LOCALS: usize = 256;

/// A local variable: its name, and the scope it belongs to.
struct Local {
    name: String,
    depth: i32,
    /// Whether a nested function captured it, so the scope's end has to close
    /// the upvalue rather than simply popping the slot.
    is_captured: bool,
}

/// Where a captured variable comes from, as the `Closure` instruction records
/// it: either a local of the immediately enclosing function, or one of *its*
/// upvalues — which is how a variable two or more levels up is reached, one
/// hop at a time.
#[derive(PartialEq, Eq)]
struct UpvalueRef {
    is_local: bool,
    index: usize,
}

/// One function being compiled.
///
/// **A stack of these rather than upstream's linked list of `Compiler`s.** The
/// same structure, but resolving an upvalue means walking a `Vec` instead of
/// following parent pointers, which in Rust would mean either lifetimes that
/// do not work out or `Rc<RefCell<_>>` on the hot path of the compiler.
struct FnState {
    chunk: Chunk,
    locals: Vec<Local>,
    upvalues: Vec<UpvalueRef>,
    scope_depth: i32,
    arity: usize,
    name: String,
    /// A constructor body returns `this` rather than null.
    is_initializer: bool,
    /// Loops being compiled in *this* function, innermost last.
    ///
    /// **Per function, not per compiler.** A shared stack let a `break` inside
    /// a function nested in a loop find the enclosing loop and emit a jump into
    /// the wrong chunk entirely — offsets from one function applied to
    /// another's code. Wren's rule is that a loop does not extend through a
    /// function boundary, and keeping the stack here is what enforces it.
    loops: Vec<LoopState>,
}

impl FnState {
    /// `receiver` names slot zero: `this` in a method, and an unreferencable
    /// empty name in a plain function so that nothing can name that slot.
    fn new(name: String, receiver: &str, is_initializer: bool) -> FnState {
        FnState {
            chunk: Chunk::new(),
            // **Depth -1, below every scope.** The receiver belongs to the
            // frame rather than to any block: a module-level block opens at
            // depth 0, and a receiver recorded at 0 was indistinguishable from
            // that block's own locals, so closing the block discarded the
            // receiver and shifted every slot after it.
            locals: alloc::vec![Local {
                name: receiver.to_string(),
                depth: -1,
                is_captured: false,
            }],
            upvalues: Vec::new(),
            scope_depth: 0,
            arity: 0,
            name,
            is_initializer,
            loops: Vec::new(),
        }
    }
}

/// The class whose body is being compiled.
struct ClassState {
    /// The class's own name, for its error messages.
    name: String,
    /// Signatures already defined, so a duplicate is caught rather than
    /// silently replacing the first one -- which would look like the earlier
    /// definition simply never ran.
    defined: Vec<String>,
    /// Field names in declaration order; the index is the field's slot.
    fields: Vec<String>,
    /// Static field names, numbered the same way. Separate from `fields`
    /// because they are separate storage: instance fields live on each
    /// instance, static ones on the class.
    static_fields: Vec<String>,
    in_static: bool,
    /// Where the class itself lives, so each method can reload it.
    variable: Variable,
}

/// A loop being compiled, so `break` and `continue` know where to go.
struct LoopState {
    /// Where `continue` jumps back to.
    start: usize,
    /// Jumps emitted by `break`, waiting for the loop's end.
    breaks: Vec<usize>,
    /// How many locals existed when the loop began.
    ///
    /// **A count, not a scope depth.** Depth looked equivalent and is not: at
    /// module level the enclosing scope is `-1` while the receiver in slot zero
    /// sits at depth `0`, so "discard everything deeper than the loop" threw
    /// away the receiver and shifted every later slot by one. The count says
    /// exactly which slots the body added.
    locals_at_start: usize,
}

/// Where a variable lives.
#[derive(Clone, Copy)]
struct Variable {
    index: usize,
    is_local: bool,
}

/// Compile `source` into a chunk that [`Vm::run`](crate::vm::Vm::run) can
/// execute.
pub fn compile(vm: &mut Vm, source: &str) -> Result<Chunk, CompileError> {
    compile_in(vm, source, 0)
}

/// Compile into a particular module's namespace.
pub fn compile_in(vm: &mut Vm, source: &str, module: usize) -> Result<Chunk, CompileError> {
    let mut compiler = Compiler::new(vm, source, module);
    compiler.advance()?;
    compiler.skip_newlines()?;

    while compiler.current.kind != TokenKind::Eof {
        compiler.declaration()?;
        compiler.skip_newlines()?;
    }

    let line = clamp_line(compiler.current.line);
    compiler.chunk_mut().emit_op(Op::End, line);
    Ok(compiler.states.pop().expect("the module's own state").chunk)
}

struct Compiler<'a> {
    vm: &'a mut Vm,
    source: &'a str,
    lexer: Lexer<'a>,
    previous: Token,
    current: Token,
    /// Functions being compiled, innermost last. There is always at least one:
    /// the module's own body.
    states: Vec<FnState>,
    /// Classes being compiled, innermost last.
    classes: Vec<ClassState>,
    /// How deep the state stack was when the innermost method body started.
    ///
    /// A field reference is a one-instruction affair only when it is *directly*
    /// in a method, because `this` is slot zero there. Inside a function nested
    /// in a method, slot zero belongs to that function, and `this` has to come
    /// through the upvalue chain.
    method_depth: usize,
    /// Parameter names parsed by the signature, waiting for the body's scope.
    pending_parameters: Vec<String>,
    /// Which module's namespace `var` at the top level writes into.
    ///
    /// Passed in rather than assumed to be zero, because compiling an imported
    /// file has to put its variables in *its* module -- that is what makes two
    /// files able to define the same name without colliding.
    module: usize,
}

impl<'a> Compiler<'a> {
    fn new(vm: &'a mut Vm, source: &'a str, module: usize) -> Compiler<'a> {
        let placeholder = Token { kind: TokenKind::Eof, start: 0, end: 0, line: 1 };
        let mut body = FnState::new("(module)".to_string(), "", false);
        // **Module level is scope -1**, where a `var` becomes a module variable
        // rather than a stack slot. Upstream uses the same sentinel.
        body.scope_depth = -1;
        Compiler {
            vm,
            source,
            lexer: Lexer::new(source),
            previous: placeholder,
            current: placeholder,
            states: alloc::vec![body],
            classes: Vec::new(),
            method_depth: 0,
            pending_parameters: Vec::new(),
            module,
        }
    }

    fn state(&self) -> &FnState {
        self.states.last().expect("a function being compiled")
    }

    fn state_mut(&mut self) -> &mut FnState {
        self.states.last_mut().expect("a function being compiled")
    }

    fn chunk_mut(&mut self) -> &mut Chunk {
        &mut self.state_mut().chunk
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
        self.state_mut().scope_depth += 1;
    }

    /// Close a scope, discarding the locals it held.
    ///
    /// Each one is a live stack slot, so leaving it would corrupt every slot
    /// index above it — this is where the compiler's idea of the stack and the
    /// VM's have to agree exactly.
    fn end_scope(&mut self) {
        let line = self.line();
        let depth = self.state().scope_depth;
        loop {
            // Slot zero is never discarded by a scope ending; it is the frame's.
            if self.state().locals.len() <= 1 {
                break;
            }
            let Some(local) = self.state().locals.last() else { break };
            if local.depth < depth {
                break;
            }
            // **A captured local cannot simply be popped.** A closure may still
            // refer to the slot, so the value has to be moved into the upvalue
            // before the slot is reused -- which is what `CloseUpvalue` does,
            // and it pops as well.
            let captured = local.is_captured;
            self.chunk_mut()
                .emit_op(if captured { Op::CloseUpvalue } else { Op::Pop }, line);
            self.state_mut().locals.pop();
        }
        self.state_mut().scope_depth -= 1;
    }

    fn resolve_local(&self, name: &str) -> Option<usize> {
        // Backwards, so an inner scope's variable shadows an outer one.
        self.state().locals.iter().rposition(|local| local.name == name)
    }

    /// Declare a local occupying the slot the value on top of the stack is in.
    ///
    /// The invariant this depends on: **when a local is declared, the stack
    /// height equals the number of locals already declared.** Every statement
    /// leaves the stack as it found it, which is what keeps that true.
    ///
    /// That invariant is the compiler's half of a contract with the VM, and it
    /// is unchecked — nothing verifies at run time that slot *n* holds what the
    /// compiler thought it would. When it is broken the symptom is not a crash
    /// at the break but a variable reading as some unrelated value much later,
    /// which is how the loop-discard and scope-end bugs presented. The
    /// disassembler in `bytecode.rs` exists for exactly this class of fault.
    fn add_local(&mut self, name: &str) -> Result<usize, CompileError> {
        // 256, upstream's `MAX_LOCALS`, which is what a `u8` slot index can
        // address. The receiver occupies slot zero, so a function body really
        // gets 255 of its own -- upstream counts the same way.
        if self.state().locals.len() >= MAX_LOCALS {
            return Err(self.error_at(
                self.previous,
                "Cannot declare more than 256 variables in one scope.",
            ));
        }
        let depth = self.state().scope_depth;
        self.state_mut()
            .locals
            .push(Local { name: name.to_string(), depth, is_captured: false });
        Ok(self.state().locals.len() - 1)
    }

    /// Find a captured variable, adding upvalues along the way.
    ///
    /// **This is the part that makes closures work across more than one level.**
    /// A variable two functions up is not reachable directly: each function in
    /// between has to capture it as an upvalue of its own, so the chain can be
    /// followed one hop at a time at run time. The recursion here builds that
    /// chain.
    ///
    /// Why a chain at all, rather than having the inner function reach straight
    /// up to the variable? Because at run time the intermediate function may
    /// still be on the stack *or* may have returned, and which one it is
    /// changes where the variable lives — a live stack slot, or a closed-over
    /// copy. Only the function that directly owns the local knows which, so
    /// each level captures from the one above it and the question is answered
    /// once per level rather than guessed at from the bottom.
    ///
    /// Marking the local captured is not bookkeeping either: it tells the
    /// enclosing scope to emit `CloseUpvalue` instead of `Pop` when the scope
    /// ends, which is what copies the value out of the slot before it is
    /// reused.
    fn resolve_upvalue(&mut self, name: &str, level: usize) -> Option<usize> {
        if level == 0 {
            return None;
        }
        let enclosing = level - 1;

        if let Some(slot) = self.states[enclosing]
            .locals
            .iter()
            .rposition(|local| local.name == name)
        {
            self.states[enclosing].locals[slot].is_captured = true;
            return Some(self.add_upvalue(level, true, slot));
        }

        let found = self.resolve_upvalue(name, enclosing)?;
        Some(self.add_upvalue(level, false, found))
    }

    /// Record an upvalue on a function, reusing one it already has.
    fn add_upvalue(&mut self, level: usize, is_local: bool, index: usize) -> usize {
        let wanted = UpvalueRef { is_local, index };
        if let Some(existing) = self.states[level]
            .upvalues
            .iter()
            .position(|upvalue| *upvalue == wanted)
        {
            return existing;
        }
        self.states[level].upvalues.push(wanted);
        self.states[level].upvalues.len() - 1
    }

    // --- declarations and statements ----------------------------------------

    fn declaration(&mut self) -> Result<(), CompileError> {
        if self.match_token(TokenKind::Import)? {
            self.import_statement()?;
        } else if self.match_token(TokenKind::Class)? {
            self.class_definition()?;
        } else if self.match_token(TokenKind::Var)? {
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

    /// `import "name"` and `import "name" for A, B as C`
    ///
    /// The bare form runs the module for its side effects. The `for` form also
    /// binds names out of it, each becoming a variable in *this* module -- a
    /// copy of the value, not a link, which is why reassigning an imported
    /// variable does not affect the module it came from.
    fn import_statement(&mut self) -> Result<(), CompileError> {
        self.consume(TokenKind::String, "Expect a string after 'import'.")?;
        let path = self.unescaped(self.previous)?;
        let line = self.line();

        let path_value = self.vm.new_string(&path);
        let Some(path_constant) = self.chunk_mut().add_constant(path_value) else {
            return Err(self.error_at(self.previous, "A function may only contain 65536 unique constants."));
        };

        self.chunk_mut().emit_op(Op::ImportModule, line);
        self.chunk_mut().emit_short(path_constant, line);
        // The module body's result is of no interest; only its variables are.
        self.chunk_mut().emit_op(Op::Pop, line);

        if !self.match_token(TokenKind::For)? {
            return Ok(());
        }

        loop {
            self.skip_newlines()?;
            self.consume(TokenKind::Name, "Expect variable name after 'for'.")?;
            let imported = self.previous.text(self.source).to_string();

            // `as` renames it on the way in, so two modules exporting the same
            // name can both be used.
            let local = if self.match_token(TokenKind::As)? {
                self.consume(TokenKind::Name, "Expect variable name after 'as'.")?;
                self.previous.text(self.source).to_string()
            } else {
                imported.clone()
            };

            let name_value = self.vm.new_string(&imported);
            let Some(name_constant) = self.chunk_mut().add_constant(name_value) else {
                return Err(self.error_at(self.previous, "A function may only contain 65536 unique constants."));
            };
            self.chunk_mut().emit_op(Op::ImportVariable, line);
            self.chunk_mut().emit_short(path_constant, line);
            self.chunk_mut().emit_short(name_constant, line);

            self.define_variable(&local, line)?;

            if !self.match_token(TokenKind::Comma)? {
                break;
            }
        }
        Ok(())
    }

    fn var_declaration(&mut self) -> Result<(), CompileError> {
        self.consume(TokenKind::Name, "Expect variable name.")?;
        let name = self.previous.text(self.source).to_string();
        let line = self.line();

        if self.match_token(TokenKind::Eq)? {
            self.skip_newlines()?;
            self.expression()?;
        } else {
            self.chunk_mut().emit_op(Op::Null, line);
        }

        if self.state().scope_depth < 0 {
            // Module level: the variable lives in the module, not on the stack.
            let index = self.vm.modules[self.module].define(&name, Value::NULL);
            self.chunk_mut().emit_op(Op::StoreModuleVar, line);
            self.chunk_mut().emit_short(index as u16, line);
            self.chunk_mut().emit_op(Op::Pop, line);
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
        if self.match_token(TokenKind::Return)? {
            return self.return_statement();
        }
        if self.match_token(TokenKind::Break)? {
            return self.break_statement();
        }
        if self.match_token(TokenKind::Continue)? {
            return self.continue_statement();
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
        let else_jump = self.chunk_mut().emit_jump(Op::JumpIf, line);
        self.skip_newlines()?;
        self.statement()?;

        if self.match_token(TokenKind::Else)? {
            let line = self.line();
            let end_jump = self.chunk_mut().emit_jump(Op::Jump, line);
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
        let loop_start = self.chunk_mut().code.len();
        self.consume(TokenKind::LeftParen, "Expect '(' after 'while'.")?;
        self.skip_newlines()?;
        self.expression()?;
        self.skip_newlines()?;
        self.consume(TokenKind::RightParen, "Expect ')' after while condition.")?;

        let line = self.line();
        let exit = self.chunk_mut().emit_jump(Op::JumpIf, line);

        self.begin_loop(loop_start);
        self.skip_newlines()?;
        self.statement()?;

        let line = self.line();
        if !self.chunk_mut().emit_loop(loop_start, line) {
            return Err(self.error_at(self.previous, "Loop body too large."));
        }
        self.patch(exit)?;
        self.end_loop()
    }

    fn begin_loop(&mut self, start: usize) {
        let locals_at_start = self.state().locals.len();
        self.state_mut()
            .loops
            .push(LoopState { start, breaks: Vec::new(), locals_at_start });
    }

    fn end_loop(&mut self) -> Result<(), CompileError> {
        let finished = self.state_mut().loops.pop().expect("a loop being compiled");
        for jump in finished.breaks {
            self.patch(jump)?;
        }
        Ok(())
    }

    /// Discard the locals a `break` or `continue` is jumping out of.
    ///
    /// The compiler's idea of the stack has to match the VM's at the jump
    /// target, and the body's locals are still in their slots at that point.
    fn discard_locals_to(&mut self, keep: usize) -> Result<(), CompileError> {
        let line = self.line();
        let mut index = self.state().locals.len();
        while index > keep {
            let captured = self.state().locals[index - 1].is_captured;
            self.chunk_mut()
                .emit_op(if captured { Op::CloseUpvalue } else { Op::Pop }, line);
            index -= 1;
        }
        Ok(())
    }

    fn break_statement(&mut self) -> Result<(), CompileError> {
        let Some(keep) = self.state().loops.last().map(|state| state.locals_at_start) else {
            return Err(self.error_at(self.previous, "Cannot use 'break' outside of a loop."));
        };
        self.discard_locals_to(keep)?;
        let line = self.line();
        let jump = self.chunk_mut().emit_jump(Op::Jump, line);
        self.state_mut().loops.last_mut().expect("a loop").breaks.push(jump);
        Ok(())
    }

    fn continue_statement(&mut self) -> Result<(), CompileError> {
        let Some((start, keep)) = self
            .state()
            .loops
            .last()
            .map(|state| (state.start, state.locals_at_start))
        else {
            return Err(self.error_at(self.previous, "Cannot use 'continue' outside of a loop."));
        };
        self.discard_locals_to(keep)?;
        let line = self.line();
        if !self.chunk_mut().emit_loop(start, line) {
            return Err(self.error_at(self.previous, "Loop body too large."));
        }
        Ok(())
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
        self.chunk_mut().emit_op(Op::Null, line);
        let iterator_slot = self.add_local("iter ")?;

        self.consume(TokenKind::RightParen, "Expect ')' after loop expression.")?;

        let loop_start = self.chunk_mut().code.len();

        // iter_ = seq_.iterate(iter_)
        let line = self.line();
        self.emit_load_local(sequence_slot, line);
        self.emit_load_local(iterator_slot, line);
        self.emit_call("iterate(_)", 1, line)?;
        self.chunk_mut().emit_op(Op::StoreLocal, line);
        self.chunk_mut().emit_byte(iterator_slot as u8, line);

        // `StoreLocal` leaves the value, and `JumpIf` consumes it -- so the
        // assignment is also the loop condition, with nothing extra emitted.
        let exit = self.chunk_mut().emit_jump(Op::JumpIf, line);

        // `continue` in a `for` goes back to the iterate call, not to the top of
        // the body -- otherwise it would loop forever on the same element.
        self.begin_loop(loop_start);

        // The loop variable is a fresh local in a scope of its own, so that a
        // closure capturing it gets this iteration's value rather than the
        // last one.
        self.begin_scope();
        self.emit_load_local(sequence_slot, line);
        self.emit_load_local(iterator_slot, line);
        self.emit_call("iteratorValue(_)", 1, line)?;
        self.add_local(&variable)?;

        self.skip_newlines()?;
        self.statement()?;
        self.end_scope();

        let line = self.line();
        if !self.chunk_mut().emit_loop(loop_start, line) {
            return Err(self.error_at(self.previous, "Loop body too large."));
        }
        self.patch(exit)?;
        self.end_loop()?;

        self.end_scope();
        Ok(())
    }

    fn expression_statement(&mut self) -> Result<(), CompileError> {
        self.expression()?;
        let line = self.line();
        self.chunk_mut().emit_op(Op::Pop, line);
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

        loop {
            // **A newline before a `.` does not end the expression.** Wren
            // allows a call chain to be broken across lines, which means
            // deciding whether a newline is a separator needs one token of
            // lookahead past it -- so the lexer is cloned and run forward, and
            // the newline is consumed only if a `.` really follows.
            if self.check(TokenKind::Line) && precedence <= Precedence::Call {
                let mut ahead = self.lexer.clone();
                let mut next = ahead.next_token();
                while next.kind == TokenKind::Line {
                    next = ahead.next_token();
                }
                if next.kind != TokenKind::Dot {
                    break;
                }
                self.skip_newlines()?;
            }

            if precedence > infix_precedence(self.current.kind) {
                break;
            }
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
                self.emit_constant(Value::num(value), line)
            }
            TokenKind::String => {
                let text = self.unescaped(token)?;
                let value = self.vm.new_string(&text);
                self.emit_constant(value, line)
            }
            TokenKind::Interpolation => self.interpolation(),
            TokenKind::True => {
                self.chunk_mut().emit_op(Op::True, line);
                Ok(())
            }
            TokenKind::False => {
                self.chunk_mut().emit_op(Op::False, line);
                Ok(())
            }
            TokenKind::Null => {
                self.chunk_mut().emit_op(Op::Null, line);
                Ok(())
            }
            TokenKind::Name => self.variable(can_assign),
            TokenKind::This => self.load_this(),
            TokenKind::Field => self.field(can_assign),
            TokenKind::StaticField => self.static_field(can_assign),
            TokenKind::Super => self.super_call(),
            TokenKind::LeftParen => {
                self.skip_newlines()?;
                self.expression()?;
                self.skip_newlines()?;
                self.consume(TokenKind::RightParen, "Expect ')' after expression.")
            }
            TokenKind::LeftBracket => self.list_literal(),
            TokenKind::LeftBrace => self.map_literal(),
            TokenKind::Minus => {
                self.parse_precedence(Precedence::Unary)?;
                self.emit_call("-", 0, line)
            }
            TokenKind::Bang => {
                self.parse_precedence(Precedence::Unary)?;
                self.emit_call("!", 0, line)
            }
            TokenKind::Tilde => {
                self.parse_precedence(Precedence::Unary)?;
                self.emit_call("~", 0, line)
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
                let jump = self.chunk_mut().emit_jump(Op::And, line);
                self.skip_newlines()?;
                self.parse_precedence(Precedence::LogicalAnd)?;
                return self.patch(jump);
            }
            TokenKind::PipePipe => {
                let jump = self.chunk_mut().emit_jump(Op::Or, line);
                self.skip_newlines()?;
                self.parse_precedence(Precedence::LogicalOr)?;
                return self.patch(jump);
            }
            TokenKind::Question => return self.conditional(),
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
            TokenKind::Is => "is",
            TokenKind::Pipe => "|",
            TokenKind::Caret => "^",
            TokenKind::Amp => "&",
            TokenKind::LtLt => "<<",
            TokenKind::GtGt => ">>",
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

    /// `condition ? then : else`
    ///
    /// Right-associative, so `a ? b : c ? d : e` groups as `a ? b : (c ? d : e)`
    /// -- which is what makes a chain of them read as a series of cases.
    fn conditional(&mut self) -> Result<(), CompileError> {
        let line = self.line();
        self.skip_newlines()?;

        let else_jump = self.chunk_mut().emit_jump(Op::JumpIf, line);
        self.parse_precedence(Precedence::Conditional)?;
        self.skip_newlines()?;
        self.consume(TokenKind::Colon, "Expect ':' after then branch of conditional operator.")?;
        self.skip_newlines()?;

        let end_jump = self.chunk_mut().emit_jump(Op::Jump, line);
        self.patch(else_jump)?;
        self.parse_precedence(Precedence::Assignment)?;
        self.patch(end_jump)
    }

    /// `receiver.name`, `receiver.name(a, b)` or `receiver.name = value`.
    fn method_call(&mut self) -> Result<(), CompileError> {
        self.consume(TokenKind::Name, "Expect method name after '.'.")?;
        let name = self.previous.text(self.source).to_string();
        let line = self.line();

        if self.match_token(TokenKind::LeftParen)? {
            let mut arity = self.argument_list()?;
            // **A block after the arguments is one more argument.** This is how
            // `list.each { |x| ... }` works: there is no block syntax in the
            // language, only a function literal in the last argument position,
            // and the signature grows a `_` to match.
            if self.match_token(TokenKind::LeftBrace)? {
                self.block_argument()?;
                arity += 1;
            }
            return self.emit_call(&signature(&name, arity), arity, line);
        }

        if self.match_token(TokenKind::LeftBrace)? {
            self.block_argument()?;
            return self.emit_call(&signature(&name, 1), 1, line);
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

        // **A subscript can take more than one argument.** `grid[x, y]` is the
        // method `[_,_]`, which is how a two-dimensional container is written
        // in Wren -- there is no separate syntax for it.
        let mut arity = 0;
        self.skip_newlines()?;
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
        self.skip_newlines()?;
        self.consume(TokenKind::RightBracket, "Expect ']' after subscript.")?;

        if self.check(TokenKind::Eq) {
            self.advance()?;
            self.skip_newlines()?;
            self.expression()?;
            return self.emit_call(&subscript_signature(arity, true), arity + 1, line);
        }
        self.emit_call(&subscript_signature(arity, false), arity, line)
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
                self.chunk_mut().emit_op(Op::StoreLocal, line);
                self.chunk_mut().emit_byte(slot as u8, line);
                return Ok(());
            }
            if let Some(slot) = self.resolve_upvalue(&name, self.states.len() - 1) {
                self.chunk_mut().emit_op(Op::StoreUpvalue, line);
                self.chunk_mut().emit_byte(slot as u8, line);
                return Ok(());
            }
            if self.is_this_call(&name) {
                // `name = value` on the receiver: a setter, for the same
                // reason and in the same order as the load above.
                return self.named_call(&name, true, line);
            }
            let index = self.module_variable(&name)?;
            self.chunk_mut().emit_op(Op::StoreModuleVar, line);
            self.chunk_mut().emit_short(index as u16, line);
            return Ok(());
        }

        // **Locals first, then the implicit receiver, then the module.** The
        // order matters and is upstream's: a class named `foo` with a method
        // named `foo` must resolve `foo` inside its own body to the *method*,
        // not to the class. Checking the module first found the class and got
        // it backwards.
        if self.resolve_local(&name).is_none()
            && self.resolve_upvalue(&name, self.states.len() - 1).is_none()
            && self.is_this_call(&name)
        {
            return self.named_call(&name, can_assign, line);
        }

        self.load_named(&name, line)
    }

    /// Is this bare name a method call on the receiver?
    fn is_this_call(&self, name: &str) -> bool {
        !self.classes.is_empty()
            && self.method_depth > 0
            && name.chars().next().is_some_and(|first| first.is_lowercase())
    }

    /// Emit a load of whatever `name` refers to: a local, a captured variable,
    /// or a module variable, in that order.
    fn load_named(&mut self, name: &str, line: u16) -> Result<(), CompileError> {
        if let Some(slot) = self.resolve_local(name) {
            self.emit_load_local(slot, line);
            return Ok(());
        }
        if let Some(slot) = self.resolve_upvalue(name, self.states.len() - 1) {
            self.chunk_mut().emit_op(Op::LoadUpvalue, line);
            self.chunk_mut().emit_byte(slot as u8, line);
            return Ok(());
        }
        let index = self.module_variable(name)?;
        self.chunk_mut().emit_op(Op::LoadModuleVar, line);
        self.chunk_mut().emit_short(index as u16, line);
        Ok(())
    }

    /// Find a module variable, declaring it implicitly if the name is
    /// capitalised.
    ///
    /// **A capitalised name that is not defined yet is assumed to be a class
    /// defined later.** That is what lets two classes refer to each other, and
    /// it is upstream's rule: a lowercase name is an error on the spot, an
    /// uppercase one is a forward reference.
    fn module_variable(&mut self, name: &str) -> Result<usize, CompileError> {
        if let Some(index) = self.vm.modules[self.module].names.find(name) {
            return Ok(index);
        }
        let capitalised = name.chars().next().is_some_and(|first| first.is_uppercase());
        if !capitalised {
            return Err(self.error_at(self.previous, "Variable is not defined."));
        }
        Ok(self.vm.modules[self.module].define(name, Value::NULL))
    }

    /// A bare name inside a class body is a call on `this`.
    ///
    /// `get(n - 1)` inside a method means `this.get(n - 1)`. Upstream applies
    /// this only to lowercase names, so that `Foo` still reads as a class
    /// rather than as a method call on the receiver.
    fn named_call(&mut self, name: &str, can_assign: bool, line: u16) -> Result<(), CompileError> {
        self.load_named("this", line)?;

        if can_assign && self.check(TokenKind::Eq) {
            self.advance()?;
            self.skip_newlines()?;
            self.expression()?;
            return self.emit_call(&format!("{name}=(_)"), 1, line);
        }

        if self.match_token(TokenKind::LeftParen)? {
            let mut arity = self.argument_list()?;
            if self.match_token(TokenKind::LeftBrace)? {
                self.block_argument()?;
                arity += 1;
            }
            return self.emit_call(&signature(name, arity), arity, line);
        }

        if self.match_token(TokenKind::LeftBrace)? {
            self.block_argument()?;
            return self.emit_call(&signature(name, 1), 1, line);
        }

        self.emit_call(name, 0, line)
    }

    fn load_this(&mut self) -> Result<(), CompileError> {
        let line = self.line();
        if self.classes.is_empty() {
            return Err(self.error_at(self.previous, "Cannot use 'this' outside of a method."));
        }
        self.load_named("this", line)
    }

    /// `_field`, inside a method of the class being compiled.
    fn field(&mut self, can_assign: bool) -> Result<(), CompileError> {
        let name = self.previous.text(self.source).to_string();
        let line = self.line();

        if self.classes.is_empty() {
            return Err(self.error_at(
                self.previous,
                "Cannot reference a field outside of a class definition.",
            ));
        }
        if self.class_state().in_static {
            return Err(self.error_at(self.previous, "Cannot use an instance field in a static method."));
        }

        let index = match self.class_state().fields.iter().position(|field| *field == name) {
            Some(index) => index,
            None => {
                let class = self.classes.last_mut().expect("a class being compiled");
                if class.fields.len() >= u8::MAX as usize {
                    return Err(self.error_at(self.previous, "A class can only have 255 fields."));
                }
                class.fields.push(name);
                class.fields.len() - 1
            }
        };

        // **Directly in a method, `this` is slot zero and the instruction can
        // say so.** Inside a function nested in a method it is not — slot zero
        // there belongs to the function — so `this` has to be loaded through
        // the upvalue chain first and the general instruction used.
        let direct = self.states.len() == self.method_depth;
        let assigning = can_assign && self.check(TokenKind::Eq);

        if !direct {
            self.load_named("this", line)?;
        }
        if assigning {
            self.advance()?;
            self.skip_newlines()?;
            self.expression()?;
            self.chunk_mut()
                .emit_op(if direct { Op::StoreFieldThis } else { Op::StoreField }, line);
        } else {
            self.chunk_mut()
                .emit_op(if direct { Op::LoadFieldThis } else { Op::LoadField }, line);
        }
        self.chunk_mut().emit_byte(index as u8, line);
        Ok(())
    }

    /// `__name`, a field on the class rather than on an instance.
    ///
    /// Unlike `_name` this works in a static method as well as an instance one,
    /// because the storage is on the class and both kinds of method know which
    /// class they belong to. It also needs no `this`, which is why there is no
    /// direct-versus-indirect split here the way there is for instance fields.
    fn static_field(&mut self, can_assign: bool) -> Result<(), CompileError> {
        let name = self.previous.text(self.source).to_string();
        let line = self.line();

        if self.classes.is_empty() {
            return Err(self.error_at(
                self.previous,
                "Cannot use a static field outside of a class definition.",
            ));
        }

        let index = match self
            .class_state()
            .static_fields
            .iter()
            .position(|field| *field == name)
        {
            Some(index) => index,
            None => {
                let class = self.classes.last_mut().expect("a class being compiled");
                if class.static_fields.len() >= u8::MAX as usize {
                    return Err(self.error_at(self.previous, "A class can only have 255 static fields."));
                }
                class.static_fields.push(name);
                class.static_fields.len() - 1
            }
        };

        if can_assign && self.check(TokenKind::Eq) {
            self.advance()?;
            self.skip_newlines()?;
            self.expression()?;
            self.chunk_mut().emit_op(Op::StoreStaticField, line);
        } else {
            self.chunk_mut().emit_op(Op::LoadStaticField, line);
        }
        self.chunk_mut().emit_byte(index as u8, line);
        Ok(())
    }

    /// A string literal's contents, with escapes decoded.
    ///
    /// A bad escape is a compile error pointing at the string it is in, which
    /// is why this is a method: the free function has no token to blame.
    fn unescaped(&self, token: Token) -> Result<String, CompileError> {
        unescape(token.text(self.source)).map_err(|message| CompileError {
            message,
            line: clamp_line(token.line),
        })
    }

    fn class_state(&self) -> &ClassState {
        self.classes.last().expect("a class being compiled")
    }

    /// `super.name(args)` or `super(args)` in a constructor.
    fn super_call(&mut self) -> Result<(), CompileError> {
        let line = self.line();
        if self.classes.is_empty() {
            return Err(self.error_at(self.previous, "Cannot use 'super' outside of a method."));
        }

        // The receiver of a super call is always `this`.
        self.load_named("this", line)?;

        if self.match_token(TokenKind::Dot)? {
            self.consume(TokenKind::Name, "Expect method name after 'super.'.")?;
            let name = self.previous.text(self.source).to_string();
            // Without parentheses this is a getter, and a getter's signature is
            // the bare name -- `super.speak`, not `super.speak()`. The same
            // distinction that `foo` and `foo()` have everywhere else applies
            // here, and missing it looked for a method the class did not have.
            if !self.check(TokenKind::LeftParen) {
                return self.emit_super(&name, 0, line);
            }
            self.advance()?;
            let arity = self.argument_list()?;
            return self.emit_super(&signature(&name, arity), arity, line);
        }

        // Bare `super(...)`: call the superclass's version of the method this
        // one is in. The name is the enclosing method's own.
        let name = self.state().name.clone();
        // The enclosing method's own name. For a constructor body that is
        // `init new(_)`, and the superclass's *constructor* is what a bare
        // `super(...)` should reach -- which is the `init` form, not `new`.
        let base = name.split('(').next().unwrap_or("").to_string();
        if !self.check(TokenKind::LeftParen) {
            return self.emit_super(&base, 0, line);
        }
        self.advance()?;
        let arity = self.argument_list()?;
        self.emit_super(&signature(&base, arity), arity, line)
    }

    /// `{}` or `{ key: value, ... }`
    ///
    /// **A `{` only means a map in expression position.** As a statement it is
    /// a block, and the parser reaches this rule only through `prefix`, which
    /// is exactly where a statement cannot start.
    ///
    /// Built the same way a list literal is: `Map.new`, then one `addCore` per
    /// entry, which returns the map so it stays on the stack.
    fn map_literal(&mut self) -> Result<(), CompileError> {
        let line = self.line();
        let index = self.module_variable("Map")?;
        self.chunk_mut().emit_op(Op::LoadModuleVar, line);
        self.chunk_mut().emit_short(index as u16, line);
        self.emit_call("new()", 0, line)?;

        self.skip_newlines()?;
        if !self.check(TokenKind::RightBrace) {
            loop {
                self.skip_newlines()?;
                if self.check(TokenKind::RightBrace) {
                    break;
                }
                self.parse_precedence(Precedence::Unary)?;
                self.consume(TokenKind::Colon, "Expect ':' after map key.")?;
                self.skip_newlines()?;
                self.expression()?;
                self.emit_call("addCore(_,_)", 2, line)?;
                self.skip_newlines()?;
                if !self.match_token(TokenKind::Comma)? {
                    break;
                }
            }
        }
        self.skip_newlines()?;
        self.consume(TokenKind::RightBrace, "Expect '}' after map entries.")
    }

    /// `[a, b, c]`
    ///
    /// Built by calling `List.new` and then `addCore(_)` per element, which is
    /// upstream's approach. It needs no opcode of its own, and `addCore`
    /// returns the list so it stays on the stack between elements.
    fn list_literal(&mut self) -> Result<(), CompileError> {
        let line = self.line();
        let Some(index) = self.vm.modules[self.module].names.find("List") else {
            return Err(self.error_at(self.previous, "List class is not defined."));
        };
        self.chunk_mut().emit_op(Op::LoadModuleVar, line);
        self.chunk_mut().emit_short(index as u16, line);
        self.emit_call("new()", 0, line)?;

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
        let head = self.unescaped(self.previous)?;
        let value = self.vm.new_string(&head);
        self.emit_constant(value, line)?;

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
                let text = self.unescaped(self.previous)?;
                let value = self.vm.new_string(&text);
                self.emit_constant(value, line)?;
                self.emit_call("+(_)", 1, line)?;
                continue;
            }

            self.consume(TokenKind::String, "Expect end of string interpolation.")?;
            let text = self.unescaped(self.previous)?;
            let value = self.vm.new_string(&text);
            self.emit_constant(value, line)?;
            self.emit_call("+(_)", 1, line)?;
            return Ok(());
        }
    }

    // --- functions ----------------------------------------------------------

    fn push_function(&mut self, name: String, receiver: &str, is_initializer: bool) {
        self.states.push(FnState::new(name, receiver, is_initializer));
    }

    /// Finish the innermost function and emit a `Closure` for it in its parent.
    fn end_function(&mut self) -> Result<(), CompileError> {
        let line = self.line();
        self.chunk_mut().emit_op(Op::End, line);

        let state = self.states.pop().expect("a function being compiled");
        let upvalues = state.upvalues;

        let function = self.vm.heap.allocate(Object::Fn(Box::new(ObjFn {
            chunk: Rc::new(state.chunk),
            arity: state.arity,
            num_upvalues: upvalues.len(),
            name: state.name,
            field_offset: 0,
            super_class: None,
            owner_class: None,
            module: self.module,
        })));

        let Some(index) = self.chunk_mut().add_constant(Value::object(function)) else {
            return Err(self.error_at(
                self.previous,
                "A function may only contain 65536 unique constants.",
            ));
        };
        self.chunk_mut().emit_op(Op::Closure, line);
        self.chunk_mut().emit_short(index, line);
        self.chunk_mut().emit_byte(upvalues.len() as u8, line);
        // **Two bytes per upvalue, right after the instruction.** Only the
        // enclosing function knows where each captured variable actually is,
        // so it has to say so here rather than at compile time of the inner
        // function.
        for upvalue in &upvalues {
            self.chunk_mut().emit_byte(u8::from(upvalue.is_local), line);
            self.chunk_mut().emit_byte(upvalue.index as u8, line);
        }
        Ok(())
    }

    /// The body of a function, method or block, after its `{`.
    ///
    /// **A body with no newline after the `{` is a single expression**, and its
    /// value is returned — `Fn.new { 1 + 2 }` is 3. With a newline it is a list
    /// of statements and returns null. Upstream draws the line in exactly the
    /// same place.
    fn finish_body(&mut self) -> Result<(), CompileError> {
        let line = self.line();

        if self.match_token(TokenKind::RightBrace)? {
            return self.finish_return(false, line);
        }

        if !self.check(TokenKind::Line) {
            self.expression()?;
            self.consume(TokenKind::RightBrace, "Expect '}' at end of block.")?;
            return self.finish_return(true, line);
        }

        self.skip_newlines()?;
        if self.match_token(TokenKind::RightBrace)? {
            return self.finish_return(false, line);
        }

        loop {
            self.declaration()?;
            self.skip_newlines()?;
            if self.check(TokenKind::RightBrace) || self.check(TokenKind::Eof) {
                break;
            }
        }
        self.consume(TokenKind::RightBrace, "Expect '}' at end of block.")?;
        self.finish_return(false, line)
    }

    fn finish_return(&mut self, is_expression_body: bool, line: u16) -> Result<(), CompileError> {
        if self.state().is_initializer {
            // A constructor returns the instance whatever its body evaluated to.
            if is_expression_body {
                self.chunk_mut().emit_op(Op::Pop, line);
            }
            self.chunk_mut().emit_op(Op::LoadLocal, line);
            self.chunk_mut().emit_byte(0, line);
        } else if !is_expression_body {
            self.chunk_mut().emit_op(Op::Null, line);
        }
        self.chunk_mut().emit_op(Op::Return, line);
        Ok(())
    }

    fn return_statement(&mut self) -> Result<(), CompileError> {
        let line = self.line();
        if self.check(TokenKind::Line) || self.check(TokenKind::RightBrace) || self.check(TokenKind::Eof) {
            // **A bare `return` in a constructor still yields the instance.**
            // An ordinary method returns null; a constructor has no other
            // answer to give, and returning null from one would hand back
            // something that was never constructed.
            if self.state().is_initializer {
                self.chunk_mut().emit_op(Op::LoadLocal, line);
                self.chunk_mut().emit_byte(0, line);
            } else {
                self.chunk_mut().emit_op(Op::Null, line);
            }
        } else {
            // A constructor always yields the instance, so returning something
            // else is a mistake rather than a choice -- the value would be
            // discarded and the instance returned anyway.
            if self.state().is_initializer {
                return Err(self.error_at(self.current, "A constructor cannot return a value."));
            }
            self.expression()?;
        }
        self.chunk_mut().emit_op(Op::Return, line);
        Ok(())
    }

    /// `{ |a, b| body }` passed as an argument.
    fn block_argument(&mut self) -> Result<(), CompileError> {
        self.push_function("(fn)".to_string(), "", false);

        if self.match_token(TokenKind::Pipe)? {
            loop {
                self.skip_newlines()?;
                self.consume(TokenKind::Name, "Expect parameter name.")?;
                let name = self.previous.text(self.source).to_string();
                self.add_local(&name)?;
                self.state_mut().arity += 1;
                if !self.match_token(TokenKind::Comma)? {
                    break;
                }
            }
            self.consume(TokenKind::Pipe, "Expect '|' after parameters.")?;
        }

        self.finish_body()?;
        self.end_function()
    }

    // --- classes ------------------------------------------------------------

    fn class_definition(&mut self) -> Result<(), CompileError> {
        self.consume(TokenKind::Name, "Expect class name.")?;
        let name = self.previous.text(self.source).to_string();
        let line = self.line();

        // The class's own name, as a constant for the `Class` instruction.
        let name_value = self.vm.new_string(&name);
        self.emit_constant(name_value, line)?;

        // The superclass, or `Object` when none is named.
        if self.match_token(TokenKind::Is)? {
            self.parse_precedence(Precedence::Call)?;
        } else {
            self.load_named("Object", line)?;
        }

        // The field count is not known until the methods have been compiled,
        // so a placeholder goes in and is patched at the end.
        self.chunk_mut().emit_op(Op::Class, line);
        let field_count_at = self.chunk_mut().code.len();
        self.chunk_mut().emit_byte(255, line);

        let variable = self.define_variable(&name, line)?;

        self.classes.push(ClassState {
            name: name.clone(),
            defined: Vec::new(),
            fields: Vec::new(),
            static_fields: Vec::new(),
            in_static: false,
            variable,
        });

        self.consume(TokenKind::LeftBrace, "Expect '{' after class declaration.")?;
        self.skip_newlines()?;

        while !self.check(TokenKind::RightBrace) && !self.check(TokenKind::Eof) {
            self.method()?;
            self.skip_newlines()?;
        }
        self.consume(TokenKind::RightBrace, "Expect '}' after class body.")?;

        let class = self.classes.pop().expect("the class being compiled");
        self.state_mut().chunk.code[field_count_at] = class.fields.len() as u8;
        Ok(())
    }

    /// Declare a variable for something just pushed onto the stack.
    fn define_variable(&mut self, name: &str, line: u16) -> Result<Variable, CompileError> {
        if self.state().scope_depth < 0 {
            let index = self.vm.modules[self.module].define(name, Value::NULL);
            self.chunk_mut().emit_op(Op::StoreModuleVar, line);
            self.chunk_mut().emit_short(index as u16, line);
            self.chunk_mut().emit_op(Op::Pop, line);
            return Ok(Variable { index, is_local: false });
        }
        let slot = self.add_local(name)?;
        Ok(Variable { index: slot, is_local: true })
    }

    fn load_variable(&mut self, variable: Variable, line: u16) {
        if variable.is_local {
            self.chunk_mut().emit_op(Op::LoadLocal, line);
            self.chunk_mut().emit_byte(variable.index as u8, line);
        } else {
            self.chunk_mut().emit_op(Op::LoadModuleVar, line);
            self.chunk_mut().emit_short(variable.index as u16, line);
        }
    }

    /// One method inside a class body.
    fn method(&mut self) -> Result<(), CompileError> {
        let is_static = self.match_token(TokenKind::Static)?;
        let is_constructor = self.match_token(TokenKind::Construct)?;
        self.classes.last_mut().expect("a class").in_static = is_static;

        let (full, arity) = self.method_signature()?;
        let line = self.line();

        // **A constructor is a named method taking parentheses.** It cannot be
        // an operator, a getter, a setter or a subscript, because every one of
        // those is called on an instance that already exists -- which is the
        // one thing a constructor does not have.
        if is_constructor {
            if is_static {
                return Err(self.error_at(self.previous, "A constructor cannot be static."));
            }
            if full.ends_with("=(_)") {
                return Err(self.error_at(self.previous, "A constructor cannot be a setter."));
            }
            if full.starts_with('[') {
                return Err(self.error_at(self.previous, "A constructor cannot be a subscript."));
            }
            if !full.contains('(') {
                return Err(self.error_at(self.previous, "A constructor cannot be a getter."));
            }
            if !full.chars().next().is_some_and(|first| first.is_alphabetic() || first == '_') {
                return Err(self.error_at(self.previous, "A constructor cannot be an operator."));
            }
        }
        // A constructor's body is an instance method under a name no program
        // can write, and `new` on the metaclass is generated to call it.
        let body_signature = if is_constructor { format!("init {full}") } else { full.clone() };

        let marker = if is_static { format!("static {body_signature}") } else { body_signature.clone() };
        if self.class_state().defined.contains(&marker) {
            return Err(self.error_at(
                self.previous,
                &format!(
                    "Class {} already defines a method '{full}'.",
                    self.class_state().name
                ),
            ));
        }
        self.classes.last_mut().expect("a class").defined.push(marker);

        self.push_function(
            body_signature.clone(),
            "this",
            is_constructor,
        );
        let depth_was = self.method_depth;
        self.method_depth = self.states.len();

        for parameter in self.take_parameters() {
            self.add_local(&parameter)?;
            self.state_mut().arity += 1;
        }

        self.consume(TokenKind::LeftBrace, "Expect '{' to begin method body.")?;
        self.finish_body()?;
        self.end_function()?;
        self.method_depth = depth_was;

        let variable = self.class_state().variable;
        self.load_variable(variable, line);
        let symbol = self.vm.method_names.ensure(&body_signature);
        self.chunk_mut()
            .emit_op(if is_static { Op::MethodStatic } else { Op::MethodInstance }, line);
        self.chunk_mut().emit_short(symbol as u16, line);

        if is_constructor {
            self.emit_constructor(&full, arity, line)?;
        }
        Ok(())
    }

    /// The `new(...)` static method a `construct` declaration implies.
    ///
    /// Three instructions: allocate an instance in place of the class, run the
    /// constructor body on it, return it. Upstream generates the same thing.
    fn emit_constructor(&mut self, full: &str, arity: usize, line: u16) -> Result<(), CompileError> {
        self.push_function(full.to_string(), "this", false);
        self.state_mut().arity = arity;
        for index in 0..arity {
            self.add_local(&format!("arg {index}"))?;
        }

        self.chunk_mut().emit_op(Op::Construct, line);
        let initializer = self.vm.method_names.ensure(&format!("init {full}"));
        self.chunk_mut().emit_op(Op::LoadLocal, line);
        self.chunk_mut().emit_byte(0, line);
        for slot in 1..=arity {
            self.chunk_mut().emit_op(Op::LoadLocal, line);
            self.chunk_mut().emit_byte(slot as u8, line);
        }
        self.chunk_mut().emit_op(Op::Call, line);
        self.chunk_mut().emit_byte(arity as u8, line);
        self.chunk_mut().emit_short(initializer as u16, line);
        self.chunk_mut().emit_op(Op::Return, line);
        self.end_function()?;

        let variable = self.class_state().variable;
        self.load_variable(variable, line);
        let symbol = self.vm.method_names.ensure(full);
        self.chunk_mut().emit_op(Op::MethodStatic, line);
        self.chunk_mut().emit_short(symbol as u16, line);
        Ok(())
    }

    /// Parse a method's name and parameter list.
    ///
    /// Returns the **finished signature** rather than a name to be assembled
    /// later: a setter, a subscript and a binary operator each build one
    /// differently, and handing back a bare name meant the caller had to guess
    /// which -- which is how `[_]=(_)` came to be registered as `[_]=(_)(_,_)`.
    fn method_signature(&mut self) -> Result<(String, usize), CompileError> {
        self.advance()?;
        let token = self.previous;

        if token.kind == TokenKind::LeftBracket {
            let parameters = self.parameter_list(TokenKind::RightBracket)?;
            let count = parameters.len();
            if count == 0 {
                return Err(self.error_at(self.previous, "Expect subscript parameters."));
            }
            self.pending_parameters = parameters;
            if self.match_token(TokenKind::Eq)? {
                let mut setter = self.parameter_list_parenthesised()?;
                self.pending_parameters.append(&mut setter);
                let arity = self.pending_parameters.len();
                return Ok((subscript_signature(count, true), arity));
            }
            return Ok((subscript_signature(count, false), count));
        }

        let name = match token.kind {
            TokenKind::Name | TokenKind::Construct => token.text(self.source).to_string(),
            kind => match operator_name(kind) {
                Some(name) => name.to_string(),
                None => return Err(self.error_at(token, "Expect method definition.")),
            },
        };

        // `name=(value)` is a setter.
        if self.match_token(TokenKind::Eq)? {
            self.pending_parameters = self.parameter_list_parenthesised()?;
            return Ok((format!("{name}=(_)"), 1));
        }

        if self.match_token(TokenKind::LeftParen)? {
            self.pending_parameters = self.parameter_list(TokenKind::RightParen)?;
            let arity = self.pending_parameters.len();
            return Ok((signature(&name, arity), arity));
        }

        // No parentheses: a getter, or a unary operator such as `-` or `!`.
        self.pending_parameters = Vec::new();
        Ok((name, 0))
    }

    fn parameter_list_parenthesised(&mut self) -> Result<Vec<String>, CompileError> {
        self.consume(TokenKind::LeftParen, "Expect '(' after method name.")?;
        self.parameter_list(TokenKind::RightParen)
    }

    /// Parameter names up to `closing`, which is consumed.
    fn parameter_list(&mut self, closing: TokenKind) -> Result<Vec<String>, CompileError> {
        let mut names = Vec::new();
        self.skip_newlines()?;
        if !self.check(closing) {
            loop {
                self.skip_newlines()?;
                self.consume(TokenKind::Name, "Expect parameter name.")?;
                names.push(self.previous.text(self.source).to_string());
                if names.len() > 16 {
                    return Err(self.error_at(self.current, "Cannot have more than 16 parameters."));
                }
                self.skip_newlines()?;
                if !self.match_token(TokenKind::Comma)? {
                    break;
                }
            }
        }
        self.skip_newlines()?;
        self.consume(closing, "Expect closing bracket after parameters.")?;
        Ok(names)
    }

    fn take_parameters(&mut self) -> Vec<String> {
        core::mem::take(&mut self.pending_parameters)
    }

    fn emit_super(&mut self, signature: &str, arity: usize, line: u16) -> Result<(), CompileError> {
        let symbol = self.vm.method_names.ensure(signature);
        self.chunk_mut().emit_op(Op::Super, line);
        self.chunk_mut().emit_byte(arity as u8, line);
        self.chunk_mut().emit_short(symbol as u16, line);
        Ok(())
    }

    // --- emitting -----------------------------------------------------------

    fn emit_constant(&mut self, value: Value, line: u16) -> Result<(), CompileError> {
        let Some(index) = self.chunk_mut().add_constant(value) else {
            return Err(self.error_at(
                self.previous,
                "A function may only contain 65536 unique constants.",
            ));
        };
        self.chunk_mut().emit_op(Op::Constant, line);
        self.chunk_mut().emit_short(index, line);
        Ok(())
    }

    fn emit_load_local(&mut self, slot: usize, line: u16) {
        self.chunk_mut().emit_op(Op::LoadLocal, line);
        self.chunk_mut().emit_byte(slot as u8, line);
    }

    fn emit_call(&mut self, signature: &str, arity: usize, line: u16) -> Result<(), CompileError> {
        let symbol = self.vm.method_names.ensure(signature);
        if symbol > u16::MAX as usize {
            return Err(self.error_at(self.previous, "Too many method names."));
        }
        self.chunk_mut().emit_op(Op::Call, line);
        self.chunk_mut().emit_byte(arity as u8, line);
        self.chunk_mut().emit_short(symbol as u16, line);
        Ok(())
    }

    fn patch(&mut self, at: usize) -> Result<(), CompileError> {
        if self.chunk_mut().patch_jump(at) {
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
    // **Always parenthesised, even at arity zero.** In Wren `foo` and `foo()`
    // are different signatures -- a getter and a method that takes nothing --
    // and collapsing them meant `fiber.call()` looked for the getter `call`
    // and did not find it. A getter is emitted by passing the bare name, not
    // by calling this.
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
        TokenKind::Question => Precedence::Conditional,
        TokenKind::PipePipe => Precedence::LogicalOr,
        TokenKind::AmpAmp => Precedence::LogicalAnd,
        TokenKind::EqEq | TokenKind::BangEq => Precedence::Equality,
        TokenKind::Is => Precedence::Is,
        TokenKind::Lt | TokenKind::Gt | TokenKind::LtEq | TokenKind::GtEq => Precedence::Comparison,
        TokenKind::Pipe => Precedence::BitwiseOr,
        TokenKind::Caret => Precedence::BitwiseXor,
        TokenKind::Amp => Precedence::BitwiseAnd,
        TokenKind::LtLt | TokenKind::GtGt => Precedence::BitwiseShift,
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
        Precedence::Assignment => Precedence::Conditional,
        Precedence::Conditional => Precedence::LogicalOr,
        Precedence::LogicalOr => Precedence::LogicalAnd,
        Precedence::LogicalAnd => Precedence::Equality,
        Precedence::Equality => Precedence::Is,
        Precedence::Is => Precedence::Comparison,
        Precedence::Comparison => Precedence::BitwiseOr,
        Precedence::BitwiseOr => Precedence::BitwiseXor,
        Precedence::BitwiseXor => Precedence::BitwiseAnd,
        Precedence::BitwiseAnd => Precedence::BitwiseShift,
        Precedence::BitwiseShift => Precedence::Range,
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
fn unescape(raw: &str) -> Result<String, String> {
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
            Some('\'') => out.push('\''),
            Some('\\') => out.push('\\'),
            Some('%') => out.push('%'),
            Some('a') => out.push('\u{7}'),
            Some('b') => out.push('\u{8}'),
            Some('e') => out.push('\u{1b}'),
            Some('f') => out.push('\u{c}'),
            Some('v') => out.push('\u{b}'),

            // `\xNN` is a raw byte, which is why it is pushed as one rather
            // than as a character: a Wren string is bytes, and `\xff` is a
            // byte that is not valid UTF-8 on its own.
            Some('x') => {
                let byte = read_hex(&mut characters, 2, "byte")?;
                // Pushed through `char` because the output is a Rust `String`;
                // a lone high byte becomes its Latin-1 character, which
                // round-trips through the string's bytes for the values a
                // program can actually write here.
                out.push(byte as u8 as char);
            }
            // `\uNNNN` and `\UNNNNNNNN` are code points.
            Some('u') => out.push(read_code_point(&mut characters, 4)?),
            Some('U') => out.push(read_code_point(&mut characters, 8)?),

            // **An unknown escape is an error, not a passthrough.** Keeping
            // both characters was quietly turning a typo into output.
            Some(other) => return Err(format!("Invalid escape character '{other}'.")),
            None => return Err("Invalid escape character ''.".to_string()),
        }
    }
    Ok(out)
}

/// Read exactly `digits` hexadecimal digits.
fn read_hex(
    characters: &mut core::str::Chars<'_>,
    digits: usize,
    what: &str,
) -> Result<u32, String> {
    let mut value = 0u32;
    for _ in 0..digits {
        let Some(character) = characters.next() else {
            return Err(format!("Incomplete {what} escape sequence."));
        };
        let Some(digit) = character.to_digit(16) else {
            return Err(format!("Invalid {what} escape sequence."));
        };
        value = value * 16 + digit;
    }
    Ok(value)
}

fn read_code_point(characters: &mut core::str::Chars<'_>, digits: usize) -> Result<char, String> {
    let value = read_hex(characters, digits, "Unicode")?;
    char::from_u32(value).ok_or_else(|| "Invalid Unicode escape sequence.".to_string())
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

/// `[_]` or `[_]=(_)`, with one `_` per subscript argument.
fn subscript_signature(arity: usize, is_setter: bool) -> String {
    let mut out = String::from("[");
    for index in 0..arity {
        if index > 0 {
            out.push(',');
        }
        out.push('_');
    }
    out.push(']');
    if is_setter {
        out.push_str("=(_)");
    }
    out
}

/// The method name an operator token declares.
///
/// Wren's operators are ordinary methods, so `+(other) { }` in a class body
/// defines the method that `a + b` calls. This is the mapping between the two.
fn operator_name(kind: TokenKind) -> Option<&'static str> {
    let name = match kind {
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
        TokenKind::Bang => "!",
        TokenKind::Tilde => "~",
        TokenKind::DotDot => "..",
        TokenKind::DotDotDot => "...",
        TokenKind::Amp => "&",
        TokenKind::Pipe => "|",
        TokenKind::Caret => "^",
        TokenKind::LtLt => "<<",
        TokenKind::GtGt => ">>",
        TokenKind::Is => "is",
        _ => return None,
    };
    Some(name)
}
