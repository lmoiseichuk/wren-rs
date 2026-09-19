//! The core library: the methods built-in types respond to.
//!
//! **Everything in Wren is a method call**, including arithmetic — `1 + 2` is
//! `1.+(2)`, dispatched on `Num` like any other message. That uniformity is why
//! the interpreter loop has one call path and no special cases for operators,
//! and it is why this file exists: the operators have to live somewhere.
//!
//! Upstream writes much of its core library *in Wren*, loaded from
//! `wren_core.wren` at start-up, with only the leaves as C primitives. That
//! costs a compile of several hundred lines on every boot — part of the 33 KB
//! of compiler stack measured on the C6, and unaffordable on an 8 KB part. Here
//! everything is a primitive. The cost is that some of it is more verbose than
//! the Wren version would be; the benefit is that starting the VM compiles
//! nothing at all.
//!
//! Error messages are upstream's, word for word, because the test suite checks
//! them.
//!
//! # The calling convention, and why it is an index
//!
//! A primitive receives `&mut Vm` and the **stack index of the receiver**, not
//! a slice of arguments. The receiver is at `at`, the first argument at
//! `at + 1`, and so on — which is why the helpers here are named `receiver` and
//! `argument(_, 1)`, matching upstream's `args[0]` and `args[1]`.
//!
//! A slice would read better and does not typecheck: the arguments live in
//! `vm.stack`, so borrowing them as a slice borrows the VM immutably for the
//! whole call, while every interesting primitive needs it mutably to allocate.
//! The alternative — copying arguments into a fixed buffer on entry — costs up
//! to 17 `Value`s of memcpy on the hottest path in the language, since every
//! `a + b` goes through here. An index costs nothing and sidesteps both.
//!
//! The price is that a primitive can read past its own arguments if it asks for
//! the wrong index. That is a bug of the same shape as reading `args[2]` of a
//! one-argument method in C, and it is caught the same way: by the signature
//! and the tests, not by the type system.

extern crate alloc;

use alloc::string::ToString;

use alloc::boxed::Box;

use alloc::vec::Vec;

use crate::handle::ObjectId;
use crate::math;
use crate::object::{
    MapEntry, Method, ObjClass, ObjFiber, ObjInstance, ObjList, ObjMap, ObjRange, ObjString,
    Object, Primitive,
};
use crate::value::Value;
use crate::vm::{RuntimeError, Switch, Vm};

/// Bind a primitive to a signature on a class.
fn define(vm: &mut Vm, class: ObjectId, signature: &str, function: Primitive) {
    let symbol = vm.method_names.ensure(signature);
    if let Some(Object::Class(class)) = vm.heap.get_mut(class) {
        class.define(symbol, Method::Primitive(function));
    }
}

/// The receiver of the call in progress.
fn receiver(vm: &Vm, at: usize) -> Value {
    vm.stack[at]
}

/// Argument `index`, counting from one as Wren's own `args[1]` does.
fn argument(vm: &Vm, at: usize, index: usize) -> Value {
    vm.stack[at + index]
}

/// An argument that must be a whole number, with upstream's wording.
///
/// **Wren distinguishes "not a number" from "not an integer"** and the suite
/// checks both messages, so this cannot collapse into one check. `noun` is the
/// word the message starts with -- "Index", "Iterator", "Count".
fn integer_argument(
    vm: &Vm,
    at: usize,
    index: usize,
    noun: &str,
) -> Result<f64, RuntimeError> {
    let value = argument(vm, at, index)
        .as_num()
        .ok_or_else(|| RuntimeError::new(alloc::format!("{noun} must be a number.")))?;
    if value != math::trunc(value) {
        return Err(RuntimeError::new(alloc::format!("{noun} must be an integer.")));
    }
    Ok(value)
}

/// Can this value be a map key?
///
/// **Only the value types**: booleans, classes, null, numbers, ranges and
/// strings. A list or an instance is excluded because Wren hashes a key by its
/// contents, and a mutable object's contents can change after it is inserted --
/// which would lose the entry rather than fail loudly.
fn is_value_type(vm: &Vm, value: Value) -> bool {
    if value.is_num() || value.is_bool() || value.is_null() {
        return true;
    }
    matches!(
        value.as_object().and_then(|id| vm.heap.get(id)),
        Some(Object::String(_) | Object::Range(_) | Object::Class(_))
    )
}

fn value_type_argument(vm: &Vm, at: usize, index: usize) -> Result<Value, RuntimeError> {
    let value = argument(vm, at, index);
    if is_value_type(vm, value) {
        return Ok(value);
    }
    Err(RuntimeError::new("Key must be a value type."))
}

/// Check that a function takes exactly the arguments a core method will pass.
fn expect_arity(vm: &Vm, function: Value, wanted: usize) -> Result<(), RuntimeError> {
    let Some(closure) = function.as_object() else {
        return Err(RuntimeError::new("Argument must be a function."));
    };
    let Some(arity) = vm.arity_of(closure) else {
        return Err(RuntimeError::new("Argument must be a function."));
    };
    if arity > wanted {
        return Err(RuntimeError::new("Function expects more arguments."));
    }
    Ok(())
}

fn number_argument(vm: &Vm, at: usize, index: usize) -> Result<f64, RuntimeError> {
    argument(vm, at, index)
        .as_num()
        .ok_or_else(|| RuntimeError::new("Right operand must be a number."))
}

/// Define a binary arithmetic operator on `Num`.
macro_rules! arithmetic {
    ($vm:expr, $class:expr, $signature:literal, $left:ident, $right:ident, $body:expr) => {
        define($vm, $class, $signature, |vm, at| {
            let $left = receiver(vm, at).as_num().unwrap_or(f64::NAN);
            let $right = number_argument(vm, at, 1)?;
            Ok($body)
        });
    };
}

/// Install the core library into a fresh VM.
pub fn install(vm: &mut Vm) {
    install_object(vm);
    install_fn(vm);
    install_fiber(vm);
    install_num(vm);
    install_bool(vm);
    install_null(vm);
    install_string(vm);
    install_string_extras(vm);
    install_num_extras(vm);
    install_sequence(vm);
    install_list(vm);
    install_list_extras(vm);
    install_map(vm);
    install_range(vm);
    install_system(vm);
}

/// `Object`: what every value responds to, because every class inherits it.
fn install_object(vm: &mut Vm) {
    let class = vm.object_class;

    // **`!anything` is `false`.** Only `Bool` and `Null` override this, which
    // is exactly why `!0` and `!""` are `false` in Wren where most languages
    // would say `true`.
    define(vm, class, "!", |_, _| Ok(Value::FALSE));

    // Identity, which the types with a richer notion of equality override:
    // `Num` compares numerically and `String` compares contents.
    define(vm, class, "==(_)", |vm, at| {
        Ok(Value::bool(receiver(vm, at).is_same(argument(vm, at, 1))))
    });
    define(vm, class, "!=(_)", |vm, at| {
        Ok(Value::bool(!receiver(vm, at).is_same(argument(vm, at, 1))))
    });

    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });

    // `a is B` walks up from `a`'s class looking for `B`, so it answers true
    // for a superclass as well as for the exact class.
    define(vm, class, "is(_)", |vm, at| {
        let Some(wanted) = argument(vm, at, 1).as_object() else {
            return Err(RuntimeError::new("Right operand must be a class."));
        };
        if !matches!(vm.heap.get(wanted), Some(Object::Class(_))) {
            return Err(RuntimeError::new("Right operand must be a class."));
        }
        let mut current = vm.class_of(receiver(vm, at));
        while let Some(class) = current {
            if class == wanted {
                return Ok(Value::TRUE);
            }
            current = match vm.heap.get(class) {
                Some(Object::Class(class)) => class.superclass,
                _ => None,
            };
        }
        Ok(Value::FALSE)
    });

    define(vm, class, "type", |vm, at| match vm.class_of(receiver(vm, at)) {
        Some(class) => Ok(Value::object(class)),
        None => Ok(Value::NULL),
    });
}

/// `Fn`: what a function literal is an instance of.
fn install_fn(vm: &mut Vm) {
    let class = vm.fn_class;

    let metaclass_name = vm.heap.allocate(Object::String(ObjString::from_text("Fn metaclass")));
    let metaclass = vm
        .heap
        .allocate(Object::Class(Box::new(ObjClass::new(metaclass_name, None))));
    if let Some(Object::Class(function)) = vm.heap.get_mut(class) {
        function.metaclass = Some(metaclass);
    }

    // `Fn.new { ... }` -- the block is already a function, so this hands it
    // back. It exists because that is how a function literal is written in
    // Wren: there is no bare block expression, only a block argument.
    define(vm, metaclass, "new(_)", |vm, at| {
        let block = argument(vm, at, 1);
        match block.as_object().map(|id| vm.heap.get(id)) {
            Some(Some(Object::Closure(_))) => Ok(block),
            _ => Err(RuntimeError::new("Argument must be a function.")),
        }
    });

    define(vm, class, "arity", |vm, at| {
        let Some(id) = receiver(vm, at).as_object() else {
            return Err(RuntimeError::new("Receiver must be a function."));
        };
        let arity = vm.arity_of(id).unwrap_or(0);
        Ok(Value::num(arity as f64))
    });

    // **One `call` per arity, because a Wren signature includes its arity.**
    // `call()` and `call(1)` are different methods, not an overload, so each
    // needs its own entry in the table.
    for arity in 0..=16usize {
        let name = signature_for("call", arity);
        define(vm, class, &name, move |vm, at| {
            let Some(closure) = receiver(vm, at).as_object() else {
                return Err(RuntimeError::new("Receiver must be a function."));
            };
            vm.call_closure(closure, at)
        });
    }
}

/// `call`, `call(_)`, `call(_,_)`, ...
fn signature_for(name: &str, arity: usize) -> alloc::string::String {
    // Parenthesised even at arity zero: `call` and `call()` are different
    // methods in Wren.
    let mut out = alloc::string::String::from(name);
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

/// `Fiber`: coroutines, and the error handling built on them.
///
/// **Wren has no `try`/`catch`.** A runtime error aborts the fiber it happened
/// in, and `fiber.try()` runs one and hands back the error rather than letting
/// it propagate. So this class is both chapters at once.
fn install_fiber(vm: &mut Vm) {
    let class = vm.fiber_class;

    let metaclass_name = vm.heap.allocate(Object::String(ObjString::from_text("Fiber metaclass")));
    let metaclass = vm
        .heap
        .allocate(Object::Class(Box::new(ObjClass::new(metaclass_name, None))));
    if let Some(Object::Class(fiber)) = vm.heap.get_mut(class) {
        fiber.metaclass = Some(metaclass);
    }

    define(vm, metaclass, "new(_)", |vm, at| {
        let function = argument(vm, at, 1);
        let Some(closure) = function.as_object() else {
            return Err(RuntimeError::new("Argument must be a function."));
        };
        if !matches!(vm.heap.get(closure), Some(Object::Closure(_))) {
            return Err(RuntimeError::new("Argument must be a function."));
        }
        // A fiber's function receives at most the one value it was resumed
        // with, so anything taking more could never be called.
        if vm.arity_of(closure).unwrap_or(0) > 1 {
            return Err(RuntimeError::new("Function cannot take more than one parameter."));
        }
        let id = vm.heap.allocate(Object::Fiber(Box::new(ObjFiber::new(closure))));
        Ok(Value::object(id))
    });

    define(vm, metaclass, "current", |vm, _| match vm.current_fiber {
        Some(id) => Ok(Value::object(id)),
        None => Ok(Value::NULL),
    });

    // `Fiber.abort(message)` raises a runtime error the way a failing
    // primitive does, so `try` catches it like any other.
    define(vm, metaclass, "abort(_)", |vm, at| {
        let message = argument(vm, at, 1);
        if message.is_null() {
            // Upstream treats aborting with null as "do not actually abort".
            return Ok(Value::NULL);
        }
        let text = vm.to_string(message);
        Err(RuntimeError::new(text))
    });

    define(vm, metaclass, "yield()", |vm, _| yield_to_caller(vm, Value::NULL));
    define(vm, metaclass, "yield(_)", |vm, at| {
        let value = argument(vm, at, 1);
        yield_to_caller(vm, value)
    });

    define(vm, class, "call()", |vm, at| switch_into(vm, at, Value::NULL, true, false));
    define(vm, class, "call(_)", |vm, at| {
        let value = argument(vm, at, 1);
        switch_into(vm, at, value, true, false)
    });
    define(vm, class, "try()", |vm, at| switch_into(vm, at, Value::NULL, true, true));
    define(vm, class, "try(_)", |vm, at| {
        let value = argument(vm, at, 1);
        switch_into(vm, at, value, true, true)
    });
    // `transfer` does not record a caller, so the fiber it leaves is not
    // resumed when the target finishes -- a jump rather than a call.
    define(vm, class, "transfer()", |vm, at| switch_into(vm, at, Value::NULL, false, false));
    define(vm, class, "transfer(_)", |vm, at| {
        let value = argument(vm, at, 1);
        switch_into(vm, at, value, false, false)
    });

    define(vm, class, "isDone", |vm, at| {
        match receiver(vm, at).as_object().and_then(|id| vm.heap.get(id)) {
            Some(Object::Fiber(fiber)) => Ok(Value::bool(fiber.done)),
            _ => Err(RuntimeError::new("Receiver must be a fiber.")),
        }
    });

    define(vm, class, "error", |vm, at| {
        match receiver(vm, at).as_object().and_then(|id| vm.heap.get(id)) {
            Some(Object::Fiber(fiber)) => Ok(fiber.error),
            _ => Err(RuntimeError::new("Receiver must be a fiber.")),
        }
    });
}

/// Ask the interpreter to continue in `fiber`.
fn switch_into(
    vm: &mut Vm,
    at: usize,
    value: Value,
    set_caller: bool,
    catching: bool,
) -> Result<Value, RuntimeError> {
    let Some(target) = receiver(vm, at).as_object() else {
        return Err(RuntimeError::new("Receiver must be a fiber."));
    };
    match vm.heap.get(target) {
        Some(Object::Fiber(fiber)) if fiber.done => {
            return Err(RuntimeError::new("Cannot call a finished fiber."));
        }
        // **`call` and `try` only.** The root fiber is the one doing the
        // calling, so resuming it that way would re-enter a live stack. But
        // `transfer` to it is exactly how a fiber hands control back for good,
        // and refusing that broke four tests that were right.
        Some(Object::Fiber(_)) if set_caller && vm.root_fiber == Some(target) => {
            return Err(RuntimeError::new("Cannot call root fiber."));
        }
        Some(Object::Fiber(_)) => {}
        _ => return Err(RuntimeError::new("Receiver must be a fiber.")),
    }
    vm.pending_switch = Some(Switch {
        target,
        value,
        set_caller,
        catching,
        finishing: false,
    });
    Ok(Value::NULL)
}

/// Suspend the running fiber and hand `value` back to whoever resumed it.
fn yield_to_caller(vm: &mut Vm, value: Value) -> Result<Value, RuntimeError> {
    let caller = vm
        .current_fiber
        .and_then(|id| match vm.heap.get(id) {
            Some(Object::Fiber(fiber)) => fiber.caller,
            _ => None,
        });
    let Some(caller) = caller else {
        return Err(RuntimeError::new("No fiber to yield to."));
    };
    vm.pending_switch = Some(Switch {
        target: caller,
        value,
        set_caller: false,
        catching: false,
        finishing: false,
    });
    Ok(Value::NULL)
}

fn install_num(vm: &mut Vm) {
    let class = vm.num_class;

    arithmetic!(vm, class, "+(_)", a, b, Value::num(a + b));
    arithmetic!(vm, class, "-(_)", a, b, Value::num(a - b));
    arithmetic!(vm, class, "*(_)", a, b, Value::num(a * b));
    arithmetic!(vm, class, "/(_)", a, b, Value::num(a / b));
    // Wren's `%` follows C's fmod: the result takes the sign of the dividend,
    // which is *not* what Rust's `rem_euclid` does.
    arithmetic!(vm, class, "%(_)", a, b, Value::num(a % b));
    arithmetic!(vm, class, "<(_)", a, b, Value::bool(a < b));
    arithmetic!(vm, class, ">(_)", a, b, Value::bool(a > b));
    arithmetic!(vm, class, "<=(_)", a, b, Value::bool(a <= b));
    arithmetic!(vm, class, ">=(_)", a, b, Value::bool(a >= b));

    // Ranges are built by operators on numbers, not by a constructor.
    //
    // **Written out rather than put through `arithmetic!`.** These allocate,
    // so their bodies need the VM -- and a body passed into the macro is
    // written at the call site, where `vm` means the *outer* VM rather than the
    // closure's parameter. Macro hygiene is right to bind it that way, and the
    // result is a closure that captures, which is not a `fn` pointer.
    define(vm, class, "..(_)", |vm, at| {
        let from = receiver(vm, at).as_num().unwrap_or(f64::NAN);
        let to = number_argument(vm, at, 1)?;
        let id = vm.heap.allocate(Object::Range(ObjRange { from, to, is_inclusive: true }));
        Ok(Value::object(id))
    });
    define(vm, class, "...(_)", |vm, at| {
        let from = receiver(vm, at).as_num().unwrap_or(f64::NAN);
        let to = number_argument(vm, at, 1)?;
        let id = vm.heap.allocate(Object::Range(ObjRange { from, to, is_inclusive: false }));
        Ok(Value::object(id))
    });

    // **Equality is not an arithmetic operator here**, because the right side
    // may be any type at all: `1 == "one"` is `false`, not an error.
    define(vm, class, "==(_)", |vm, at| {
        let left = receiver(vm, at).as_num();
        let right = argument(vm, at, 1).as_num();
        Ok(Value::bool(match (left, right) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }))
    });
    define(vm, class, "!=(_)", |vm, at| {
        let left = receiver(vm, at).as_num();
        let right = argument(vm, at, 1).as_num();
        Ok(Value::bool(match (left, right) {
            (Some(a), Some(b)) => a != b,
            _ => true,
        }))
    });

    define(vm, class, "-", |vm, at| {
        Ok(Value::num(-receiver(vm, at).as_num().unwrap_or(f64::NAN)))
    });
    define(vm, class, "abs", |vm, at| {
        Ok(Value::num(math::abs(receiver(vm, at).as_num().unwrap_or(f64::NAN))))
    });
    define(vm, class, "floor", |vm, at| {
        Ok(Value::num(math::floor(receiver(vm, at).as_num().unwrap_or(f64::NAN))))
    });
    define(vm, class, "ceil", |vm, at| {
        Ok(Value::num(math::ceil(receiver(vm, at).as_num().unwrap_or(f64::NAN))))
    });
    define(vm, class, "sqrt", |vm, at| {
        Ok(Value::num(math::sqrt(receiver(vm, at).as_num().unwrap_or(f64::NAN))))
    });
    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });
}

fn install_bool(vm: &mut Vm) {
    let class = vm.bool_class;
    define(vm, class, "!", |vm, at| Ok(Value::bool(receiver(vm, at).is_falsy())));
    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });
    define(vm, class, "==(_)", |vm, at| {
        Ok(Value::bool(receiver(vm, at).is_same(argument(vm, at, 1))))
    });
    define(vm, class, "!=(_)", |vm, at| {
        Ok(Value::bool(!receiver(vm, at).is_same(argument(vm, at, 1))))
    });
}

fn install_null(vm: &mut Vm) {
    let class = vm.null_class;
    // `!null` is `true`: null is falsy, and this is the only way a program can
    // ask about it without a comparison.
    define(vm, class, "!", |_, _| Ok(Value::TRUE));
    define(vm, class, "toString", |vm, _| Ok(vm.new_string("null")));
    define(vm, class, "==(_)", |vm, at| {
        Ok(Value::bool(argument(vm, at, 1).is_null()))
    });
    define(vm, class, "!=(_)", |vm, at| {
        Ok(Value::bool(!argument(vm, at, 1).is_null()))
    });
}

fn install_string(vm: &mut Vm) {
    let class = vm.string_class;

    define(vm, class, "+(_)", |vm, at| {
        let left = vm.to_string(receiver(vm, at));
        let right = argument(vm, at, 1);
        // Upstream requires the right operand to be a string too, rather than
        // coercing it. `"a" + 1` is an error in Wren, and quietly making it
        // work here would be a different language.
        if vm.string_at(right).is_none() {
            return Err(RuntimeError::new("Right operand must be a string."));
        }
        let joined = alloc::format!("{}{}", left, vm.to_string(right));
        Ok(vm.new_string(&joined))
    });

    define(vm, class, "count", |vm, at| {
        // Bytes, as upstream's `count` is. A string's length in *characters*
        // needs decoding and is a different method there.
        let text = receiver(vm, at);
        let length = match vm.heap.get(text.as_object().unwrap()) {
            Some(Object::String(string)) => string.bytes.len(),
            _ => 0,
        };
        Ok(Value::num(length as f64))
    });

    define(vm, class, "toString", |vm, at| Ok(receiver(vm, at)));

    define(vm, class, "==(_)", |vm, at| {
        Ok(Value::bool(strings_equal(vm, receiver(vm, at), argument(vm, at, 1))))
    });
    define(vm, class, "!=(_)", |vm, at| {
        Ok(Value::bool(!strings_equal(vm, receiver(vm, at), argument(vm, at, 1))))
    });
}

/// Wren compares strings by **contents**, not by identity.
fn strings_equal(vm: &Vm, left: Value, right: Value) -> bool {
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return false;
    };
    match (vm.heap.get(left), vm.heap.get(right)) {
        (Some(Object::String(a)), Some(Object::String(b))) => {
            // The cached hash is a cheap rejection before comparing bytes.
            a.hash() == b.hash() && a.bytes == b.bytes
        }
        _ => false,
    }
}

/// The rest of `Num`.
fn install_num_extras(vm: &mut Vm) {
    let class = vm.num_class;

    define(vm, class, "min(_)", |vm, at| {
        let a = receiver(vm, at).as_num().unwrap_or(f64::NAN);
        let b = number_argument(vm, at, 1)?;
        Ok(Value::num(if a < b { a } else { b }))
    });
    define(vm, class, "max(_)", |vm, at| {
        let a = receiver(vm, at).as_num().unwrap_or(f64::NAN);
        let b = number_argument(vm, at, 1)?;
        Ok(Value::num(if a > b { a } else { b }))
    });
    define(vm, class, "clamp(_,_)", |vm, at| {
        let value = receiver(vm, at).as_num().unwrap_or(f64::NAN);
        let low = number_argument(vm, at, 1)?;
        let high = number_argument(vm, at, 2)?;
        let clamped = if value < low {
            low
        } else if value > high {
            high
        } else {
            value
        };
        Ok(Value::num(clamped))
    });

    define(vm, class, "truncate", |vm, at| {
        Ok(Value::num(math::trunc(receiver(vm, at).as_num().unwrap_or(f64::NAN))))
    });
    define(vm, class, "fraction", |vm, at| {
        let value = receiver(vm, at).as_num().unwrap_or(f64::NAN);
        Ok(Value::num(value - math::trunc(value)))
    });
    define(vm, class, "sign", |vm, at| {
        let value = receiver(vm, at).as_num().unwrap_or(f64::NAN);
        let sign = if value > 0.0 {
            1.0
        } else if value < 0.0 {
            -1.0
        } else {
            // Zero's sign is zero, not one. Wren follows the sign function
            // rather than `copysign`.
            0.0
        };
        Ok(Value::num(sign))
    });
    define(vm, class, "isInteger", |vm, at| {
        let value = receiver(vm, at).as_num().unwrap_or(f64::NAN);
        Ok(Value::bool(value.is_finite() && value == math::trunc(value)))
    });
    define(vm, class, "isNan", |vm, at| {
        Ok(Value::bool(receiver(vm, at).as_num().unwrap_or(0.0).is_nan()))
    });
    define(vm, class, "isInfinity", |vm, at| {
        Ok(Value::bool(receiver(vm, at).as_num().unwrap_or(0.0).is_infinite()))
    });

    // **Wren's bitwise operators work on 32-bit unsigned values**, so a double
    // is truncated and wrapped first and the result comes back as a double.
    // Anything else would make `~0` depend on the width of a C int.
    define(vm, class, "&(_)", |vm, at| bitwise(vm, at, |a, b| a & b));
    define(vm, class, "|(_)", |vm, at| bitwise(vm, at, |a, b| a | b));
    define(vm, class, "^(_)", |vm, at| bitwise(vm, at, |a, b| a ^ b));
    define(vm, class, "<<(_)", |vm, at| bitwise(vm, at, |a, b| a.wrapping_shl(b & 31)));
    define(vm, class, ">>(_)", |vm, at| bitwise(vm, at, |a, b| a.wrapping_shr(b & 31)));
    define(vm, class, "~", |vm, at| {
        let value = receiver(vm, at).as_num().unwrap_or(f64::NAN);
        Ok(Value::num(!(value as i64 as u32) as f64))
    });

    // **Only with `std`.** These need libm, which a `no_std` firmware does not
    // have and this crate will not take a dependency for. Leaving them
    // undefined there gives "Num does not implement 'sin'", which is honest;
    // defining them to return NaN would be a silent wrong answer.
    #[cfg(feature = "std")]
    {
        define(vm, class, "round", |vm, at| {
            Ok(Value::num(receiver(vm, at).as_num().unwrap_or(f64::NAN).round()))
        });
        define(vm, class, "pow(_)", |vm, at| {
            let base = receiver(vm, at).as_num().unwrap_or(f64::NAN);
            let exponent = number_argument(vm, at, 1)?;
            Ok(Value::num(base.powf(exponent)))
        });
        define(vm, class, "log", |vm, at| {
            Ok(Value::num(receiver(vm, at).as_num().unwrap_or(f64::NAN).ln()))
        });
        define(vm, class, "log2", |vm, at| {
            Ok(Value::num(receiver(vm, at).as_num().unwrap_or(f64::NAN).log2()))
        });
        define(vm, class, "exp", |vm, at| {
            Ok(Value::num(receiver(vm, at).as_num().unwrap_or(f64::NAN).exp()))
        });
        define(vm, class, "cbrt", |vm, at| {
            Ok(Value::num(receiver(vm, at).as_num().unwrap_or(f64::NAN).cbrt()))
        });
        define(vm, class, "sin", |vm, at| {
            Ok(Value::num(receiver(vm, at).as_num().unwrap_or(f64::NAN).sin()))
        });
        define(vm, class, "cos", |vm, at| {
            Ok(Value::num(receiver(vm, at).as_num().unwrap_or(f64::NAN).cos()))
        });
        define(vm, class, "tan", |vm, at| {
            Ok(Value::num(receiver(vm, at).as_num().unwrap_or(f64::NAN).tan()))
        });
        define(vm, class, "asin", |vm, at| {
            Ok(Value::num(receiver(vm, at).as_num().unwrap_or(f64::NAN).asin()))
        });
        define(vm, class, "acos", |vm, at| {
            Ok(Value::num(receiver(vm, at).as_num().unwrap_or(f64::NAN).acos()))
        });
        define(vm, class, "atan", |vm, at| {
            Ok(Value::num(receiver(vm, at).as_num().unwrap_or(f64::NAN).atan()))
        });
        define(vm, class, "atan(_)", |vm, at| {
            let y = receiver(vm, at).as_num().unwrap_or(f64::NAN);
            let x = number_argument(vm, at, 1)?;
            Ok(Value::num(y.atan2(x)))
        });
    }

    let metaclass_name = vm.heap.allocate(Object::String(ObjString::from_text("Num metaclass")));
    let metaclass = vm
        .heap
        .allocate(Object::Class(Box::new(ObjClass::new(metaclass_name, None))));
    if let Some(Object::Class(num)) = vm.heap.get_mut(class) {
        num.metaclass = Some(metaclass);
    }
    define(vm, metaclass, "pi", |_, _| Ok(Value::num(core::f64::consts::PI)));
    define(vm, metaclass, "e", |_, _| Ok(Value::num(core::f64::consts::E)));
    define(vm, metaclass, "infinity", |_, _| Ok(Value::num(f64::INFINITY)));
    define(vm, metaclass, "nan", |_, _| Ok(Value::num(f64::NAN)));
    define(vm, metaclass, "largest", |_, _| Ok(Value::num(f64::MAX)));
    define(vm, metaclass, "smallest", |_, _| Ok(Value::num(f64::MIN_POSITIVE)));
    define(vm, metaclass, "maxSafeInteger", |_, _| Ok(Value::num(9007199254740991.0)));
    define(vm, metaclass, "minSafeInteger", |_, _| Ok(Value::num(-9007199254740991.0)));
}

fn bitwise(vm: &Vm, at: usize, operation: fn(u32, u32) -> u32) -> Result<Value, RuntimeError> {
    let left = receiver(vm, at).as_num().unwrap_or(f64::NAN);
    let right = argument(vm, at, 1)
        .as_num()
        .ok_or_else(|| RuntimeError::new("Right operand must be a number."))?;
    Ok(Value::num(operation(left as i64 as u32, right as i64 as u32) as f64))
}

/// The rest of `String`.
fn install_string_extras(vm: &mut Vm) {
    let class = vm.string_class;

    define(vm, class, "[_]", |vm, at| {
        let text = string_bytes(vm, receiver(vm, at));
        // A range subscript takes a substring, as it slices a list.
        if let Some(range) = range_of(vm, argument(vm, at, 1)) {
            let taken = slice_indices(&range, text.len())?;
            let bytes: Vec<u8> = taken.into_iter().map(|index| text[index]).collect();
            let sliced = alloc::string::String::from_utf8_lossy(&bytes).into_owned();
            return Ok(vm.new_string(&sliced));
        }
        let index = number_argument(vm, at, 1)?;
        let index = resolve_index(index, text.len())?;
        // **Indexing is by byte but yields a whole code point.** Upstream does
        // the same: a string is bytes, but `s[0]` of a multi-byte character is
        // that character rather than half of it.
        let rest = alloc::string::String::from_utf8_lossy(&text[index..]).into_owned();
        let character: alloc::string::String = rest.chars().take(1).collect();
        Ok(vm.new_string(&character))
    });

    define(vm, class, "contains(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let needle = string_text(vm, argument(vm, at, 1));
        Ok(Value::bool(text.contains(&needle)))
    });
    define(vm, class, "startsWith(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let needle = string_text(vm, argument(vm, at, 1));
        Ok(Value::bool(text.starts_with(&needle)))
    });
    define(vm, class, "endsWith(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let needle = string_text(vm, argument(vm, at, 1));
        Ok(Value::bool(text.ends_with(&needle)))
    });
    define(vm, class, "indexOf(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let needle = string_text(vm, argument(vm, at, 1));
        Ok(Value::num(text.find(&needle).map_or(-1.0, |index| index as f64)))
    });

    define(vm, class, "isEmpty", |vm, at| {
        Ok(Value::bool(string_bytes(vm, receiver(vm, at)).is_empty()))
    });

    define(vm, class, "trim()", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let trimmed = text.trim().to_string();
        Ok(vm.new_string(&trimmed))
    });
    define(vm, class, "trimStart()", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let trimmed = text.trim_start().to_string();
        Ok(vm.new_string(&trimmed))
    });
    define(vm, class, "trimEnd()", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let trimmed = text.trim_end().to_string();
        Ok(vm.new_string(&trimmed))
    });

    define(vm, class, "split(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let separator = string_text(vm, argument(vm, at, 1));
        if separator.is_empty() {
            return Err(RuntimeError::new("Separator cannot be empty."));
        }
        let parts: Vec<alloc::string::String> =
            text.split(&separator).map(|part| part.to_string()).collect();
        let values: Vec<Value> = parts.iter().map(|part| vm.new_string(part)).collect();
        Ok(new_list(vm, values))
    });

    define(vm, class, "*(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let count = number_argument(vm, at, 1)?;
        if count < 0.0 || count != math::trunc(count) {
            return Err(RuntimeError::new("Count must be a non-negative integer."));
        }
        let repeated = text.repeat(count as usize);
        Ok(vm.new_string(&repeated))
    });

    // The byte and code-point views. Upstream returns lazy sequences; these are
    // lists, which answers `count`, `[_]` and iteration the same way.
    define(vm, class, "bytes", |vm, at| {
        let values: Vec<Value> = string_bytes(vm, receiver(vm, at))
            .iter()
            .map(|byte| Value::num(*byte as f64))
            .collect();
        Ok(new_list(vm, values))
    });
    define(vm, class, "codePoints", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let values: Vec<Value> =
            text.chars().map(|character| Value::num(character as u32 as f64)).collect();
        Ok(new_list(vm, values))
    });

    // Iterating a string yields its characters, one code point at a time.
    define(vm, class, "iterate(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let current = argument(vm, at, 1);
        let start = if current.is_null() {
            0
        } else {
            let index = current
                .as_num()
                .ok_or_else(|| RuntimeError::new("Iterator must be a number."))?
                as usize;
            // Step past the character that starts at this byte.
            match text[index..].chars().next() {
                Some(character) => index + character.len_utf8(),
                None => return Ok(Value::FALSE),
            }
        };
        if start >= text.len() {
            return Ok(Value::FALSE);
        }
        Ok(Value::num(start as f64))
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let index = number_argument(vm, at, 1)? as usize;
        let character: alloc::string::String = text[index..].chars().take(1).collect();
        Ok(vm.new_string(&character))
    });

    define(vm, class, "<(_)", |vm, at| compare_strings(vm, at, |o| o < 0));
    define(vm, class, ">(_)", |vm, at| compare_strings(vm, at, |o| o > 0));
    define(vm, class, "<=(_)", |vm, at| compare_strings(vm, at, |o| o <= 0));
    define(vm, class, ">=(_)", |vm, at| compare_strings(vm, at, |o| o >= 0));

    let metaclass_name = vm.heap.allocate(Object::String(ObjString::from_text("String metaclass")));
    let metaclass = vm
        .heap
        .allocate(Object::Class(Box::new(ObjClass::new(metaclass_name, None))));
    if let Some(Object::Class(string)) = vm.heap.get_mut(class) {
        string.metaclass = Some(metaclass);
    }
    define(vm, metaclass, "fromCodePoint(_)", |vm, at| {
        let point = number_argument(vm, at, 1)?;
        let Some(character) = char::from_u32(point as u32) else {
            return Err(RuntimeError::new("Code point cannot be greater than 0x10ffff."));
        };
        let text = alloc::string::String::from(character);
        Ok(vm.new_string(&text))
    });
    define(vm, metaclass, "fromByte(_)", |vm, at| {
        let byte = number_argument(vm, at, 1)?;
        if !(0.0..=255.0).contains(&byte) || byte != math::trunc(byte) {
            return Err(RuntimeError::new("Byte must be an integer between 0 and 255."));
        }
        let id = vm
            .heap
            .allocate(Object::String(ObjString::new(alloc::vec![byte as u8])));
        Ok(Value::object(id))
    });
}

fn compare_strings(
    vm: &Vm,
    at: usize,
    accept: fn(i32) -> bool,
) -> Result<Value, RuntimeError> {
    let left = string_bytes(vm, receiver(vm, at));
    let right = match argument(vm, at, 1).as_object().and_then(|id| vm.heap.get(id)) {
        Some(Object::String(text)) => text.bytes.clone(),
        _ => return Err(RuntimeError::new("Right operand must be a string.")),
    };
    let ordering = match left.cmp(&right) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    };
    Ok(Value::bool(accept(ordering)))
}

fn string_bytes(vm: &Vm, value: Value) -> Vec<u8> {
    match value.as_object().and_then(|id| vm.heap.get(id)) {
        Some(Object::String(text)) => text.bytes.clone(),
        _ => Vec::new(),
    }
}

fn string_text(vm: &Vm, value: Value) -> alloc::string::String {
    alloc::string::String::from_utf8_lossy(&string_bytes(vm, value)).into_owned()
}

/// `Sequence`: everything that can be iterated, and everything that follows
/// from being able to.
///
/// **The whole contract is `iterate(_)` and `iteratorValue(_)`.** A type that
/// answers those two gets `count`, `map`, `where`, `reduce`, `join` and the
/// rest for free, including a class written in Wren -- which is why these are
/// built on [`Vm::invoke_with`] rather than on any particular representation.
///
/// `List` overrides several of them with direct versions, because going
/// through the protocol to read an element it could index costs a method call
/// per element and lists are where that shows.
fn install_sequence(vm: &mut Vm) {
    let class = vm.sequence_class;

    define(vm, class, "count", |vm, at| {
        let sequence = receiver(vm, at);
        let mut count = 0.0;
        let mut iterator = Value::NULL;
        loop {
            iterator = vm.invoke_with(sequence, "iterate(_)", &[iterator])?;
            if iterator.is_falsy() {
                return Ok(Value::num(count));
            }
            count += 1.0;
        }
    });

    define(vm, class, "isEmpty", |vm, at| {
        let sequence = receiver(vm, at);
        let first = vm.invoke_with(sequence, "iterate(_)", &[Value::NULL])?;
        Ok(Value::bool(first.is_falsy()))
    });

    define(vm, class, "each(_)", |vm, at| {
        let sequence = receiver(vm, at);
        let function = argument(vm, at, 1);
        for element in collect(vm, sequence)? {
            vm.call_function(function, &[element])?;
        }
        Ok(Value::NULL)
    });

    define(vm, class, "toList", |vm, at| {
        let sequence = receiver(vm, at);
        let elements = collect(vm, sequence)?;
        Ok(new_list(vm, elements))
    });

    define(vm, class, "contains(_)", |vm, at| {
        let sequence = receiver(vm, at);
        let wanted = argument(vm, at, 1);
        for element in collect(vm, sequence)? {
            if values_equal(vm, element, wanted) {
                return Ok(Value::TRUE);
            }
        }
        Ok(Value::FALSE)
    });

    define(vm, class, "all(_)", |vm, at| {
        let sequence = receiver(vm, at);
        let function = function_argument(vm, at, 1)?;
        for element in collect(vm, sequence)? {
            if vm.call_function(function, &[element])?.is_falsy() {
                return Ok(Value::FALSE);
            }
        }
        Ok(Value::TRUE)
    });

    define(vm, class, "any(_)", |vm, at| {
        let sequence = receiver(vm, at);
        let function = function_argument(vm, at, 1)?;
        for element in collect(vm, sequence)? {
            if !vm.call_function(function, &[element])?.is_falsy() {
                return Ok(Value::TRUE);
            }
        }
        Ok(Value::FALSE)
    });

    define(vm, class, "reduce(_)", |vm, at| {
        let sequence = receiver(vm, at);
        let function = argument(vm, at, 1);
        expect_arity(vm, function, 2)?;
        let mut elements = collect(vm, sequence)?.into_iter();
        let Some(mut total) = elements.next() else {
            return Err(RuntimeError::new("Can't reduce an empty sequence."));
        };
        for element in elements {
            total = vm.call_function(function, &[total, element])?;
        }
        Ok(total)
    });

    define(vm, class, "reduce(_,_)", |vm, at| {
        let sequence = receiver(vm, at);
        let mut total = argument(vm, at, 1);
        let function = argument(vm, at, 2);
        expect_arity(vm, function, 2)?;
        for element in collect(vm, sequence)? {
            total = vm.call_function(function, &[total, element])?;
        }
        Ok(total)
    });

    define(vm, class, "join()", |vm, at| {
        let sequence = receiver(vm, at);
        let joined = join_sequence(vm, sequence, "")?;
        Ok(vm.new_string(&joined))
    });
    define(vm, class, "join(_)", |vm, at| {
        let sequence = receiver(vm, at);
        let separator = argument(vm, at, 1);
        if vm.string_at(separator).is_none() {
            return Err(RuntimeError::new("Right operand must be a string."));
        }
        let separator = vm.to_string(separator);
        let joined = join_sequence(vm, sequence, &separator)?;
        Ok(vm.new_string(&joined))
    });

    // **The lazy four.** `map` on an infinite sequence has to stay infinite,
    // so these build a view rather than a list. Upstream's own test drives
    // `map` with a Fibonacci iterator that never ends.
    define(vm, class, "map(_)", |vm, at| {
        let function = function_argument(vm, at, 1)?;
        let class = vm.map_sequence_class;
        Ok(new_view(vm, class, &[receiver(vm, at), function]))
    });
    define(vm, class, "where(_)", |vm, at| {
        let function = function_argument(vm, at, 1)?;
        let class = vm.where_sequence_class;
        Ok(new_view(vm, class, &[receiver(vm, at), function]))
    });
    define(vm, class, "take(_)", |vm, at| {
        let count = counting_argument(vm, at, 1)?;
        let class = vm.take_sequence_class;
        Ok(new_view(vm, class, &[receiver(vm, at), Value::num(count), Value::num(0.0)]))
    });
    define(vm, class, "skip(_)", |vm, at| {
        let count = counting_argument(vm, at, 1)?;
        let class = vm.skip_sequence_class;
        Ok(new_view(vm, class, &[receiver(vm, at), Value::num(count)]))
    });

    install_lazy_sequences(vm);
}

/// The lazy views. Each holds its source and delegates the protocol to it.
fn install_lazy_sequences(vm: &mut Vm) {
    // `map`: same iteration, transformed values.
    let class = vm.map_sequence_class;
    define(vm, class, "iterate(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let iterator = argument(vm, at, 1);
        vm.invoke_with(source, "iterate(_)", &[iterator])
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let function = instance_field(vm, receiver(vm, at), 1);
        let iterator = argument(vm, at, 1);
        let value = vm.invoke_with(source, "iteratorValue(_)", &[iterator])?;
        vm.call_function(function, &[value])
    });

    // `where`: same values, advancing past the ones that do not match.
    let class = vm.where_sequence_class;
    define(vm, class, "iterate(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let function = instance_field(vm, receiver(vm, at), 1);
        let mut iterator = argument(vm, at, 1);
        loop {
            iterator = vm.invoke_with(source, "iterate(_)", &[iterator])?;
            if iterator.is_falsy() {
                return Ok(Value::FALSE);
            }
            let value = vm.invoke_with(source, "iteratorValue(_)", &[iterator])?;
            if !vm.call_function(function, &[value])?.is_falsy() {
                return Ok(iterator);
            }
        }
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let iterator = argument(vm, at, 1);
        vm.invoke_with(source, "iteratorValue(_)", &[iterator])
    });

    // `take`: stops after a count, which it has to remember between calls --
    // hence the third field rather than deriving it from the iterator, which
    // for an arbitrary sequence is not a number this can count with.
    let class = vm.take_sequence_class;
    define(vm, class, "iterate(_)", |vm, at| {
        let this = receiver(vm, at);
        let source = instance_field(vm, this, 0);
        let limit = instance_field(vm, this, 1).as_num().unwrap_or(0.0);
        let iterator = argument(vm, at, 1);

        let taken = if iterator.is_null() {
            1.0
        } else {
            instance_field(vm, this, 2).as_num().unwrap_or(0.0) + 1.0
        };
        set_instance_field(vm, this, 2, Value::num(taken));
        if taken > limit {
            return Ok(Value::FALSE);
        }
        vm.invoke_with(source, "iterate(_)", &[iterator])
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let iterator = argument(vm, at, 1);
        vm.invoke_with(source, "iteratorValue(_)", &[iterator])
    });

    // `skip`: burns through the first n on the first call only.
    let class = vm.skip_sequence_class;
    define(vm, class, "iterate(_)", |vm, at| {
        let this = receiver(vm, at);
        let source = instance_field(vm, this, 0);
        let mut iterator = argument(vm, at, 1);

        if !iterator.is_null() {
            return vm.invoke_with(source, "iterate(_)", &[iterator]);
        }
        let mut remaining = instance_field(vm, this, 1).as_num().unwrap_or(0.0);
        iterator = vm.invoke_with(source, "iterate(_)", &[iterator])?;
        while remaining > 0.0 && !iterator.is_falsy() {
            iterator = vm.invoke_with(source, "iterate(_)", &[iterator])?;
            remaining -= 1.0;
        }
        Ok(iterator)
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let iterator = argument(vm, at, 1);
        vm.invoke_with(source, "iteratorValue(_)", &[iterator])
    });
}

/// Walk a sequence into a list, through the protocol.
///
/// **This is what makes an infinite sequence hang**, which is correct: asking
/// an endless sequence for its elements is a question with no answer, and the
/// lazy views exist precisely so the question is not asked.
fn collect(vm: &mut Vm, sequence: Value) -> Result<Vec<Value>, RuntimeError> {
    let mut elements = Vec::new();
    let mut iterator = Value::NULL;
    loop {
        iterator = vm.invoke_with(sequence, "iterate(_)", &[iterator])?;
        if iterator.is_falsy() {
            return Ok(elements);
        }
        elements.push(vm.invoke_with(sequence, "iteratorValue(_)", &[iterator])?);
    }
}

fn join_sequence(
    vm: &mut Vm,
    sequence: Value,
    separator: &str,
) -> Result<alloc::string::String, RuntimeError> {
    let mut out = alloc::string::String::new();
    for (index, element) in collect(vm, sequence)?.into_iter().enumerate() {
        if index > 0 {
            out.push_str(separator);
        }
        out.push_str(&vm.stringify(element)?);
    }
    Ok(out)
}

fn new_view(vm: &mut Vm, class: crate::handle::ObjectId, fields: &[Value]) -> Value {
    let id = vm.heap.allocate(Object::Instance(ObjInstance {
        class,
        fields: fields.to_vec(),
    }));
    Value::object(id)
}

fn set_instance_field(vm: &mut Vm, value: Value, index: usize, to: Value) {
    if let Some(Object::Instance(instance)) = value.as_object().and_then(|id| vm.heap.get_mut(id)) {
        if instance.fields.len() <= index {
            instance.fields.resize(index + 1, Value::NULL);
        }
        instance.fields[index] = to;
    }
}

fn function_argument(vm: &Vm, at: usize, index: usize) -> Result<Value, RuntimeError> {
    let value = argument(vm, at, index);
    match value.as_object().map(|id| vm.heap.get(id)) {
        Some(Some(Object::Closure(_))) => Ok(value),
        _ => Err(RuntimeError::new("Argument must be a function.")),
    }
}

/// A count for `take` or `skip`: a non-negative whole number.
fn counting_argument(vm: &Vm, at: usize, index: usize) -> Result<f64, RuntimeError> {
    let count = integer_argument(vm, at, index, "Count")?;
    if count < 0.0 {
        return Err(RuntimeError::new("Count must be a non-negative integer."));
    }
    Ok(count)
}

fn install_list(vm: &mut Vm) {
    let class = vm.list_class;

    // `List` has to be reachable by name, because a list literal compiles to
    // `List.new` followed by an `addCore(_)` per element rather than to an
    // opcode of its own.
    let metaclass_name = vm.heap.allocate(Object::String(ObjString::from_text("List metaclass")));
    let metaclass = vm
        .heap
        .allocate(Object::Class(Box::new(ObjClass::new(metaclass_name, None))));
    if let Some(Object::Class(list)) = vm.heap.get_mut(class) {
        list.metaclass = Some(metaclass);
    }
    define(vm, metaclass, "new()", |vm, _| {
        Ok(new_list(vm, Vec::new()))
    });
    vm.modules[0].define("List", Value::object(class));

    // Like `add(_)`, but returns the *list* rather than the element, so a list
    // literal can add each element without reloading the list between them.
    define(vm, class, "addCore(_)", |vm, at| {
        let list = receiver(vm, at);
        let element = argument(vm, at, 1);
        match vm.heap.get_mut(list.as_object().unwrap()) {
            Some(Object::List(list)) => list.elements.push(element),
            _ => return Err(RuntimeError::new("Receiver must be a list.")),
        }
        Ok(list)
    });

    define(vm, class, "add(_)", |vm, at| {
        let list = receiver(vm, at);
        let element = argument(vm, at, 1);
        match vm.heap.get_mut(list.as_object().unwrap()) {
            Some(Object::List(list)) => list.elements.push(element),
            _ => return Err(RuntimeError::new("Receiver must be a list.")),
        }
        // `add` returns the element, which is what makes `list.add(x)` usable
        // as an expression.
        Ok(element)
    });

    define(vm, class, "count", |vm, at| {
        Ok(Value::num(list_length(vm, receiver(vm, at)) as f64))
    });

    define(vm, class, "[_]", |vm, at| {
        let list = receiver(vm, at);
        let length = list_length(vm, list);

        // **A range subscript is a slice**, which is why this cannot simply
        // demand a number: `list[1..2]` is a list, `list[1]` is an element.
        if let Some(range) = range_of(vm, argument(vm, at, 1)) {
            let taken = slice_indices(&range, length)?;
            let elements = list_elements(vm, list);
            let sliced = taken.into_iter().map(|index| elements[index]).collect();
            return Ok(new_list(vm, sliced));
        }

        let index = number_argument(vm, at, 1)?;
        let index = resolve_index(index, length)?;
        match vm.heap.get(list.as_object().unwrap()) {
            Some(Object::List(list)) => Ok(list.elements[index]),
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    define(vm, class, "[_]=(_)", |vm, at| {
        let list = receiver(vm, at);
        let index = number_argument(vm, at, 1)?;
        let value = argument(vm, at, 2);
        let length = list_length(vm, list);
        let index = resolve_index(index, length)?;
        match vm.heap.get_mut(list.as_object().unwrap()) {
            Some(Object::List(list)) => {
                list.elements[index] = value;
                Ok(value)
            }
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    // The iteration protocol. `for (x in list)` compiles to calls to these two,
    // so a type is iterable exactly when it answers them — there is no separate
    // iterator object and nothing to allocate per loop.
    define(vm, class, "iterate(_)", |vm, at| {
        let length = list_length(vm, receiver(vm, at));
        if length == 0 {
            return Ok(Value::FALSE);
        }
        let current = argument(vm, at, 1);
        if current.is_null() {
            return Ok(Value::num(0.0));
        }
        let index = integer_argument(vm, at, 1, "Iterator")?;
        if index < 0.0 || index >= (length - 1) as f64 {
            return Ok(Value::FALSE);
        }
        Ok(Value::num(index + 1.0))
    });

    define(vm, class, "iteratorValue(_)", |vm, at| {
        let list = receiver(vm, at);
        let index = number_argument(vm, at, 1)?;
        let length = list_length(vm, list);
        let index = resolve_index(index, length)?;
        match vm.heap.get(list.as_object().unwrap()) {
            Some(Object::List(list)) => Ok(list.elements[index]),
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    define(vm, class, "toString", |vm, at| {
        // **Each element through its own `toString`**, not through the VM's
        // formatter: a class defining `toString` should have it used inside a
        // list too, which is what upstream's core/list/to_string checks.
        let mut out = alloc::string::String::from("[");
        for (index, element) in list_elements(vm, receiver(vm, at)).into_iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&vm.stringify(element)?);
        }
        out.push(']');
        Ok(vm.new_string(&out))
    });
}

/// The rest of `List`, and the `Sequence` methods it inherits upstream.
///
/// **Upstream writes these in Wren**, in `wren_core.wren`, as methods on a
/// `Sequence` class that `List`, `Map`, `Range` and `String` all inherit. That
/// costs a compile of several hundred lines at every start-up — 33 KB of
/// compiler stack on the C6 — so here they are primitives on the types that
/// need them.
fn install_list_extras(vm: &mut Vm) {
    let class = vm.list_class;

    define(vm, class, "toList", |vm, at| Ok(receiver(vm, at)));

    define(vm, class, "insert(_,_)", |vm, at| {
        let list = receiver(vm, at);
        let index = integer_argument(vm, at, 1, "Index")?;
        let value = argument(vm, at, 2);
        let length = list_length(vm, list);
        // `insert` accepts one past the end, where the other index-taking
        // methods do not: appending is a legitimate insertion point.
        let at_index = if index < 0.0 { index + length as f64 + 1.0 } else { index };
        if at_index < 0.0 || at_index > length as f64 {
            return Err(RuntimeError::new("Index out of bounds."));
        }
        match vm.heap.get_mut(list.as_object().unwrap()) {
            Some(Object::List(list)) => {
                list.elements.insert(at_index as usize, value);
                Ok(value)
            }
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    define(vm, class, "+(_)", |vm, at| {
        let mut joined = list_elements(vm, receiver(vm, at));
        let other = argument(vm, at, 1);
        if !matches!(other.as_object().and_then(|id| vm.heap.get(id)), Some(Object::List(_))) {
            return Err(RuntimeError::new("Right operand must be a list."));
        }
        joined.extend(list_elements(vm, other));
        Ok(new_list(vm, joined))
    });

    define(vm, class, "removeAt(_)", |vm, at| {
        let list = receiver(vm, at);
        let index = number_argument(vm, at, 1)?;
        let length = list_length(vm, list);
        let index = resolve_index(index, length)?;
        match vm.heap.get_mut(list.as_object().unwrap()) {
            Some(Object::List(list)) => Ok(list.elements.remove(index)),
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    define(vm, class, "clear()", |vm, at| {
        if let Some(Object::List(list)) =
            receiver(vm, at).as_object().and_then(|id| vm.heap.get_mut(id))
        {
            list.elements.clear();
        }
        Ok(Value::NULL)
    });

    define(vm, class, "contains(_)", |vm, at| {
        let wanted = argument(vm, at, 1);
        let found = list_elements(vm, receiver(vm, at))
            .iter()
            .any(|element| values_equal(vm, *element, wanted));
        Ok(Value::bool(found))
    });

    define(vm, class, "indexOf(_)", |vm, at| {
        let wanted = argument(vm, at, 1);
        let found = list_elements(vm, receiver(vm, at))
            .iter()
            .position(|element| values_equal(vm, *element, wanted));
        Ok(Value::num(found.map_or(-1.0, |index| index as f64)))
    });

    define(vm, class, "isEmpty", |vm, at| {
        Ok(Value::bool(list_length(vm, receiver(vm, at)) == 0))
    });

    define(vm, class, "each(_)", |vm, at| {
        let function = argument(vm, at, 1);
        for element in list_elements(vm, receiver(vm, at)) {
            vm.call_function(function, &[element])?;
        }
        Ok(Value::NULL)
    });

    define(vm, class, "map(_)", |vm, at| {
        let function = argument(vm, at, 1);
        let mut mapped = Vec::new();
        for element in list_elements(vm, receiver(vm, at)) {
            mapped.push(vm.call_function(function, &[element])?);
        }
        Ok(new_list(vm, mapped))
    });

    define(vm, class, "where(_)", |vm, at| {
        let function = argument(vm, at, 1);
        let mut kept = Vec::new();
        for element in list_elements(vm, receiver(vm, at)) {
            if !vm.call_function(function, &[element])?.is_falsy() {
                kept.push(element);
            }
        }
        Ok(new_list(vm, kept))
    });

    define(vm, class, "any(_)", |vm, at| {
        let function = argument(vm, at, 1);
        for element in list_elements(vm, receiver(vm, at)) {
            if !vm.call_function(function, &[element])?.is_falsy() {
                return Ok(Value::TRUE);
            }
        }
        Ok(Value::FALSE)
    });

    define(vm, class, "all(_)", |vm, at| {
        let function = argument(vm, at, 1);
        for element in list_elements(vm, receiver(vm, at)) {
            if vm.call_function(function, &[element])?.is_falsy() {
                return Ok(Value::FALSE);
            }
        }
        Ok(Value::TRUE)
    });

    define(vm, class, "reduce(_)", |vm, at| {
        let function = argument(vm, at, 1);
        expect_arity(vm, function, 2)?;
        let elements = list_elements(vm, receiver(vm, at));
        let mut iterator = elements.into_iter();
        let Some(mut total) = iterator.next() else {
            return Err(RuntimeError::new("Can't reduce an empty sequence."));
        };
        for element in iterator {
            total = vm.call_function(function, &[total, element])?;
        }
        Ok(total)
    });

    define(vm, class, "reduce(_,_)", |vm, at| {
        let mut total = argument(vm, at, 1);
        let function = argument(vm, at, 2);
        expect_arity(vm, function, 2)?;
        for element in list_elements(vm, receiver(vm, at)) {
            total = vm.call_function(function, &[total, element])?;
        }
        Ok(total)
    });

    define(vm, class, "join()", |vm, at| {
        let joined = join_elements(vm, receiver(vm, at), "")?;
        Ok(vm.new_string(&joined))
    });

    define(vm, class, "join(_)", |vm, at| {
        let separator = argument(vm, at, 1);
        if vm.string_at(separator).is_none() {
            return Err(RuntimeError::new("Right operand must be a string."));
        }
        let separator = vm.to_string(separator);
        let joined = join_elements(vm, receiver(vm, at), &separator)?;
        Ok(vm.new_string(&joined))
    });

    // `List.new()` and `List.filled(n, value)`.
    let metaclass = match vm.heap.get(class) {
        Some(Object::Class(list)) => list.metaclass,
        _ => None,
    };
    if let Some(metaclass) = metaclass {
        define(vm, metaclass, "filled(_,_)", |vm, at| {
            let count = number_argument(vm, at, 1)?;
            if count < 0.0 || count != math::trunc(count) {
                return Err(RuntimeError::new("Size must be a non-negative integer."));
            }
            let value = argument(vm, at, 2);
            Ok(new_list(vm, alloc::vec![value; count as usize]))
        });
    }
}

fn list_elements(vm: &Vm, list: Value) -> Vec<Value> {
    match list.as_object().and_then(|id| vm.heap.get(id)) {
        Some(Object::List(list)) => list.elements.clone(),
        _ => Vec::new(),
    }
}

fn join_elements(vm: &mut Vm, list: Value, separator: &str) -> Result<alloc::string::String, RuntimeError> {
    let mut out = alloc::string::String::new();
    for (index, element) in list_elements(vm, list).into_iter().enumerate() {
        if index > 0 {
            out.push_str(separator);
        }
        out.push_str(&vm.stringify(element)?);
    }
    Ok(out)
}

fn list_length(vm: &Vm, list: Value) -> usize {
    match list.as_object().and_then(|id| vm.heap.get(id)) {
        Some(Object::List(list)) => list.elements.len(),
        _ => 0,
    }
}

/// Turn a possibly-negative index into a real one, as Wren does.
///
/// `list[-1]` is the last element. Upstream's message for an out-of-range index
/// is exactly this, and the suite checks it.
fn resolve_index(index: f64, length: usize) -> Result<usize, RuntimeError> {
    if index != math::trunc(index) {
        return Err(RuntimeError::new("Index must be an integer."));
    }
    let resolved = if index < 0.0 { index + length as f64 } else { index };
    if resolved < 0.0 || resolved >= length as f64 {
        return Err(RuntimeError::new("Index out of bounds."));
    }
    Ok(resolved as usize)
}

/// `Map`, and the `MapEntry` its iteration yields.
fn install_map(vm: &mut Vm) {
    let class = vm.map_class;

    let metaclass_name = vm.heap.allocate(Object::String(ObjString::from_text("Map metaclass")));
    let metaclass = vm
        .heap
        .allocate(Object::Class(Box::new(ObjClass::new(metaclass_name, None))));
    if let Some(Object::Class(map)) = vm.heap.get_mut(class) {
        map.metaclass = Some(metaclass);
    }
    define(vm, metaclass, "new()", |vm, _| {
        let id = vm.heap.allocate(Object::Map(ObjMap::new()));
        Ok(Value::object(id))
    });

    // What a map literal emits per entry; returns the map so the next entry
    // does not have to reload it.
    define(vm, class, "addCore(_,_)", |vm, at| {
        let map = receiver(vm, at);
        let key = argument(vm, at, 1);
        let value = argument(vm, at, 2);
        map_set(vm, map, key, value)?;
        Ok(map)
    });

    define(vm, class, "[_]", |vm, at| {
        let map = receiver(vm, at);
        let key = value_type_argument(vm, at, 1)?;
        // **A missing key is null, not an error.** That is Wren's rule and it
        // is what makes `map[k] ?: default` idiomatic.
        Ok(map_get(vm, map, key).unwrap_or(Value::NULL))
    });

    define(vm, class, "[_]=(_)", |vm, at| {
        let map = receiver(vm, at);
        let key = value_type_argument(vm, at, 1)?;
        let value = argument(vm, at, 2);
        map_set(vm, map, key, value)?;
        Ok(value)
    });

    define(vm, class, "count", |vm, at| {
        Ok(Value::num(map_entries(vm, receiver(vm, at)).len() as f64))
    });

    define(vm, class, "containsKey(_)", |vm, at| {
        let map = receiver(vm, at);
        let key = value_type_argument(vm, at, 1)?;
        Ok(Value::bool(map_get(vm, map, key).is_some()))
    });

    define(vm, class, "remove(_)", |vm, at| {
        let map = receiver(vm, at);
        let key = value_type_argument(vm, at, 1)?;
        let Some(id) = map.as_object() else {
            return Err(RuntimeError::new("Receiver must be a map."));
        };
        let found = map_index(vm, map, key);
        match (found, vm.heap.get_mut(id)) {
            (Some(index), Some(Object::Map(map))) => Ok(map.entries.remove(index).value),
            _ => Ok(Value::NULL),
        }
    });

    define(vm, class, "clear()", |vm, at| {
        if let Some(Object::Map(map)) = receiver(vm, at).as_object().and_then(|id| vm.heap.get_mut(id))
        {
            map.entries.clear();
        }
        Ok(Value::NULL)
    });

    define(vm, class, "toString", |vm, at| {
        // `{}` when empty, `{key: value, ...}` otherwise -- the literal syntax,
        // so what a map prints can be pasted back into a program.
        let entries = map_entries(vm, receiver(vm, at));
        let mut out = alloc::string::String::from("{");
        for (index, entry) in entries.into_iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&vm.stringify(entry.key)?);
            out.push_str(": ");
            out.push_str(&vm.stringify(entry.value)?);
        }
        out.push('}');
        Ok(vm.new_string(&out))
    });

    define(vm, class, "keys", |vm, at| {
        let keys: Vec<Value> = map_entries(vm, receiver(vm, at)).iter().map(|e| e.key).collect();
        Ok(new_list(vm, keys))
    });
    define(vm, class, "values", |vm, at| {
        let values: Vec<Value> = map_entries(vm, receiver(vm, at)).iter().map(|e| e.value).collect();
        Ok(new_list(vm, values))
    });

    // Iterating a map yields `MapEntry` objects, so `for (e in map)` can reach
    // both halves through `e.key` and `e.value`.
    define(vm, class, "iterate(_)", |vm, at| {
        let count = map_entries(vm, receiver(vm, at)).len();
        if count == 0 {
            return Ok(Value::FALSE);
        }
        let current = argument(vm, at, 1);
        if current.is_null() {
            return Ok(Value::num(0.0));
        }
        let index = integer_argument(vm, at, 1, "Iterator")?;
        if index < 0.0 || index >= (count - 1) as f64 {
            return Ok(Value::FALSE);
        }
        Ok(Value::num(index + 1.0))
    });

    define(vm, class, "iteratorValue(_)", |vm, at| {
        let entries = map_entries(vm, receiver(vm, at));
        let index = number_argument(vm, at, 1)? as usize;
        let Some(entry) = entries.get(index).copied() else {
            return Err(RuntimeError::new("Index out of bounds."));
        };
        let class = vm.map_entry_class;
        let id = vm.heap.allocate(Object::Instance(ObjInstance {
            class,
            fields: alloc::vec![entry.key, entry.value],
        }));
        Ok(Value::object(id))
    });

    // `MapEntry` itself: two fields, reachable by name.
    let entry_class = vm.map_entry_class;
    define(vm, entry_class, "key", |vm, at| Ok(instance_field(vm, receiver(vm, at), 0)));
    define(vm, entry_class, "value", |vm, at| Ok(instance_field(vm, receiver(vm, at), 1)));
    define(vm, entry_class, "toString", |vm, at| {
        let key = instance_field(vm, receiver(vm, at), 0);
        let value = instance_field(vm, receiver(vm, at), 1);
        let text = alloc::format!("{}:{}", vm.to_string(key), vm.to_string(value));
        Ok(vm.new_string(&text))
    });
}

fn instance_field(vm: &Vm, value: Value, index: usize) -> Value {
    match value.as_object().and_then(|id| vm.heap.get(id)) {
        Some(Object::Instance(instance)) => {
            instance.fields.get(index).copied().unwrap_or(Value::NULL)
        }
        _ => Value::NULL,
    }
}

fn map_entries(vm: &Vm, map: Value) -> Vec<MapEntry> {
    match map.as_object().and_then(|id| vm.heap.get(id)) {
        Some(Object::Map(map)) => map.entries.clone(),
        _ => Vec::new(),
    }
}

/// Where `key` sits in the map, by Wren's equality rather than by identity.
fn map_index(vm: &Vm, map: Value, key: Value) -> Option<usize> {
    let entries = match map.as_object().and_then(|id| vm.heap.get(id)) {
        Some(Object::Map(map)) => &map.entries,
        _ => return None,
    };
    entries.iter().position(|entry| values_equal(vm, entry.key, key))
}

fn map_get(vm: &Vm, map: Value, key: Value) -> Option<Value> {
    let index = map_index(vm, map, key)?;
    match map.as_object().and_then(|id| vm.heap.get(id)) {
        Some(Object::Map(map)) => map.entries.get(index).map(|entry| entry.value),
        _ => None,
    }
}

fn map_set(vm: &mut Vm, map: Value, key: Value, value: Value) -> Result<(), RuntimeError> {
    let Some(id) = map.as_object() else {
        return Err(RuntimeError::new("Receiver must be a map."));
    };
    let existing = map_index(vm, map, key);
    match vm.heap.get_mut(id) {
        Some(Object::Map(map)) => {
            match existing {
                Some(index) => map.entries[index].value = value,
                None => map.entries.push(MapEntry { key, value }),
            }
            Ok(())
        }
        _ => Err(RuntimeError::new("Receiver must be a map.")),
    }
}

/// Wren's `==` for the types that can be map keys.
///
/// **Strings compare by contents**, which is the whole reason this is not just
/// a bit comparison: two separately allocated `"a"` are the same key.
pub fn values_equal(vm: &Vm, left: Value, right: Value) -> bool {
    if left.is_same(right) {
        return true;
    }
    if let (Some(a), Some(b)) = (left.as_num(), right.as_num()) {
        return a == b;
    }
    // **Ranges compare by value.** Two separately built `1..3` are the same
    // range, and since a range is a legal map key, comparing them by identity
    // meant `map[1..3]` could never find what `map[1..3] = x` had put there.
    if let (Some(a), Some(b)) = (range_of(vm, left), range_of(vm, right)) {
        return a.from == b.from && a.to == b.to && a.is_inclusive == b.is_inclusive;
    }
    strings_equal(vm, left, right)
}

fn install_range(vm: &mut Vm) {
    let class = vm.range_class;

    define(vm, class, "from", |vm, at| {
        Ok(Value::num(range_of(vm, receiver(vm, at)).map_or(f64::NAN, |r| r.from)))
    });
    define(vm, class, "to", |vm, at| {
        Ok(Value::num(range_of(vm, receiver(vm, at)).map_or(f64::NAN, |r| r.to)))
    });
    define(vm, class, "min", |vm, at| {
        let range = range_of(vm, receiver(vm, at));
        Ok(Value::num(range.map_or(f64::NAN, |r| r.from.min(r.to))))
    });
    define(vm, class, "max", |vm, at| {
        let range = range_of(vm, receiver(vm, at));
        Ok(Value::num(range.map_or(f64::NAN, |r| r.from.max(r.to))))
    });

    // Upstream's `Range.iterate` verbatim in behaviour, including the two edge
    // cases that are easy to get wrong: an exclusive empty range, and a
    // descending range counting down.
    define(vm, class, "iterate(_)", |vm, at| {
        let Some(range) = range_of(vm, receiver(vm, at)) else {
            return Err(RuntimeError::new("Receiver must be a range."));
        };

        if range.from == range.to && !range.is_inclusive {
            return Ok(Value::FALSE);
        }

        let current = argument(vm, at, 1);
        if current.is_null() {
            return Ok(Value::num(range.from));
        }
        let mut iterator = integer_argument(vm, at, 1, "Iterator")?;

        if range.from < range.to {
            iterator += 1.0;
            if iterator > range.to {
                return Ok(Value::FALSE);
            }
        } else {
            iterator -= 1.0;
            if iterator < range.to {
                return Ok(Value::FALSE);
            }
        }

        if !range.is_inclusive && iterator == range.to {
            return Ok(Value::FALSE);
        }

        Ok(Value::num(iterator))
    });

    // For a range the iterator *is* the value, which is why iterating one
    // allocates nothing at all.
    define(vm, class, "iteratorValue(_)", |vm, at| Ok(argument(vm, at, 1)));

    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });
}

/// The element indices a range selects, in the order it selects them.
///
/// Handles the two things that make slicing fiddly: a descending range reads
/// backwards, and an exclusive range stops one short of its end.
fn slice_indices(range: &ObjRange, length: usize) -> Result<Vec<usize>, RuntimeError> {
    let resolve = |value: f64| -> Result<isize, RuntimeError> {
        if value != math::trunc(value) {
            return Err(RuntimeError::new("Range start must be an integer."));
        }
        let resolved = if value < 0.0 { value + length as f64 } else { value };
        Ok(resolved as isize)
    };

    let from = resolve(range.from)?;
    let to = resolve(range.to)?;
    let descending = to < from;

    let last = if range.is_inclusive {
        to
    } else if descending {
        to + 1
    } else {
        to - 1
    };

    // An exclusive empty range selects nothing rather than failing.
    if !range.is_inclusive && from == to {
        return Ok(Vec::new());
    }
    if from < 0 || from >= length as isize {
        return Err(RuntimeError::new("Range start out of bounds."));
    }
    if last < 0 || last >= length as isize {
        return Err(RuntimeError::new("Range end out of bounds."));
    }

    let mut indices = Vec::new();
    if descending {
        let mut index = from;
        while index >= last {
            indices.push(index as usize);
            index -= 1;
        }
    } else {
        let mut index = from;
        while index <= last {
            indices.push(index as usize);
            index += 1;
        }
    }
    Ok(indices)
}

fn range_of(vm: &Vm, value: Value) -> Option<ObjRange> {
    match vm.heap.get(value.as_object()?)? {
        Object::Range(range) => Some(*range),
        _ => None,
    }
}

/// `System`, and the metaclass that holds its static methods.
fn install_system(vm: &mut Vm) {
    let metaclass_name = vm.heap.allocate(Object::String(ObjString::from_text("System metaclass")));
    let metaclass = vm
        .heap
        .allocate(Object::Class(Box::new(ObjClass::new(metaclass_name, None))));

    let name = vm.heap.allocate(Object::String(ObjString::from_text("System")));
    let mut class = ObjClass::new(name, None);
    class.metaclass = Some(metaclass);
    let system = vm.heap.allocate(Object::Class(Box::new(class)));

    define(vm, metaclass, "print(_)", |vm, at| {
        let value = argument(vm, at, 1);
        let text = vm.stringify(value)?;
        vm.output.extend_from_slice(text.as_bytes());
        vm.output.push(b'\n');
        // `System.print(x)` returns x, so it can be dropped into an expression.
        Ok(value)
    });

    define(vm, metaclass, "print", |vm, _| {
        vm.output.push(b'\n');
        Ok(Value::NULL)
    });

    define(vm, metaclass, "write(_)", |vm, at| {
        let value = argument(vm, at, 1);
        let text = vm.stringify(value)?;
        vm.output.extend_from_slice(text.as_bytes());
        Ok(value)
    });

    vm.modules[0].define("System", Value::object(system));

    // **The core classes have to be reachable by name.** `class Foo {}` with no
    // `is` clause compiles to a load of `Object`, and `x is Num` needs `Num`.
    for (name, class) in [
        ("Object", vm.object_class),
        ("Bool", vm.bool_class),
        ("Class", vm.class_class),
        ("Fn", vm.fn_class),
        ("List", vm.list_class),
        ("Map", vm.map_class),
        ("Null", vm.null_class),
        ("Num", vm.num_class),
        ("Fiber", vm.fiber_class),
        ("Range", vm.range_class),
        ("Sequence", vm.sequence_class),
        ("String", vm.string_class),
    ] {
        vm.modules[0].define(name, Value::object(class));
    }
}

/// Build the built-in `random` module and return its index.
///
/// **A native module rather than Wren source.** Upstream ships `random` as a
/// `.wren` file compiled at start-up; compiling it on a part with 8 KB is the
/// thing this implementation is trying not to do, so the class is built
/// directly.
///
/// The generator is xorshift128, not upstream's WELL512a. Upstream's tests ask
/// for statistical properties -- "roughly evenly distributed", "within range"
/// -- and never for a particular sequence, so matching the exact algorithm
/// would buy nothing. If a program ever needs to reproduce upstream's stream
/// from a seed, that is the point at which WELL512a becomes worth writing.
pub fn install_random(vm: &mut Vm) -> usize {
    let name = vm.heap.allocate(Object::String(ObjString::from_text("Random")));
    let class = vm
        .heap
        .allocate(Object::Class(Box::new(ObjClass::new(name, Some(vm.object_class)))));

    let metaclass_name = vm.heap.allocate(Object::String(ObjString::from_text("Random metaclass")));
    let metaclass = vm
        .heap
        .allocate(Object::Class(Box::new(ObjClass::new(metaclass_name, None))));
    if let Some(Object::Class(random)) = vm.heap.get_mut(class) {
        random.metaclass = Some(metaclass);
        // Four `u32` words of state, each exactly representable as an `f64`.
        random.num_fields = 4;
    }

    define(vm, metaclass, "new()", |vm, _| {
        let class = vm.random_class;
        let seed = default_seed();
        Ok(new_random(vm, class, seed))
    });
    define(vm, metaclass, "new(_)", |vm, at| {
        let seed = number_argument(vm, at, 1)? as i64 as u32;
        let class = vm.random_class;
        Ok(new_random(vm, class, seed))
    });

    define(vm, class, "float()", |vm, at| {
        let value = next_float(vm, receiver(vm, at));
        Ok(Value::num(value))
    });
    define(vm, class, "float(_)", |vm, at| {
        let end = number_argument(vm, at, 1)?;
        let value = next_float(vm, receiver(vm, at));
        Ok(Value::num(value * end))
    });
    define(vm, class, "float(_,_)", |vm, at| {
        let start = number_argument(vm, at, 1)?;
        let end = number_argument(vm, at, 2)?;
        let value = next_float(vm, receiver(vm, at));
        Ok(Value::num(start + value * (end - start)))
    });

    define(vm, class, "int()", |vm, at| {
        let value = next_u32(vm, receiver(vm, at));
        Ok(Value::num(value as f64))
    });
    define(vm, class, "int(_)", |vm, at| {
        let end = number_argument(vm, at, 1)?;
        let value = next_float(vm, receiver(vm, at));
        Ok(Value::num(math::floor(value * end)))
    });
    define(vm, class, "int(_,_)", |vm, at| {
        let start = number_argument(vm, at, 1)?;
        let end = number_argument(vm, at, 2)?;
        let value = next_float(vm, receiver(vm, at));
        Ok(Value::num(start + math::floor(value * (end - start))))
    });

    define(vm, class, "sample(_)", |vm, at| {
        let list = argument(vm, at, 1);
        let elements = list_elements(vm, list);
        if elements.is_empty() {
            return Err(RuntimeError::new("Not enough elements to sample."));
        }
        let pick = next_float(vm, receiver(vm, at));
        let index = (pick * elements.len() as f64) as usize;
        Ok(elements[index.min(elements.len() - 1)])
    });

    define(vm, class, "sample(_,_)", |vm, at| {
        let list = argument(vm, at, 1);
        let count = number_argument(vm, at, 2)?;
        let mut elements = list_elements(vm, list);
        if count < 0.0 || count as usize > elements.len() {
            return Err(RuntimeError::new("Not enough elements to sample."));
        }
        // **Without replacement**, which is what Wren's `sample` promises: a
        // partial Fisher-Yates, taking the first `count` after shuffling only
        // as far as needed.
        let wanted = count as usize;
        let receiver = receiver(vm, at);
        for index in 0..wanted {
            let pick = next_float(vm, receiver);
            let last = elements.len() - 1;
            let choice = index + (pick * (elements.len() - index) as f64) as usize;
            elements.swap(index, choice.min(last));
        }
        elements.truncate(wanted);
        Ok(new_list(vm, elements))
    });

    define(vm, class, "shuffle(_)", |vm, at| {
        let list = argument(vm, at, 1);
        let mut elements = list_elements(vm, list);
        if elements.is_empty() {
            return Ok(list);
        }
        let receiver = receiver(vm, at);
        // Fisher-Yates, downward, which is the version that needs no rejection
        // and touches each element once.
        let mut index = elements.len() - 1;
        while index > 0 {
            let pick = next_float(vm, receiver);
            let choice = (pick * (index + 1) as f64) as usize;
            elements.swap(index, choice.min(index));
            index -= 1;
        }
        if let Some(Object::List(target)) = list.as_object().and_then(|id| vm.heap.get_mut(id)) {
            target.elements = elements;
        }
        Ok(list)
    });

    vm.random_class = class;

    let mut module = crate::vm::Module::new();
    module.define("Random", Value::object(class));
    vm.modules.push(module);
    let index = vm.modules.len() - 1;
    vm.module_index.insert(alloc::string::String::from("random"), index);
    index
}

fn new_random(vm: &mut Vm, class: crate::handle::ObjectId, seed: u32) -> Value {
    // Spread one seed word across four state words. A state of all zeros is
    // the one xorshift cannot escape, so the constants guarantee it never is.
    let mut state = [
        seed ^ 0x9e37_79b9,
        seed.wrapping_mul(0x85eb_ca6b) | 1,
        seed.wrapping_mul(0xc2b2_ae35) ^ 0x1234_5678,
        seed.rotate_left(16) | 0x8000_0000,
    ];
    // Discard the first few outputs, which are the most correlated with the
    // seed.
    for _ in 0..8 {
        step(&mut state);
    }
    let fields = state.iter().map(|word| Value::num(*word as f64)).collect();
    let id = vm.heap.allocate(Object::Instance(ObjInstance { class, fields }));
    Value::object(id)
}

/// xorshift128, one step.
fn step(state: &mut [u32; 4]) -> u32 {
    let mut t = state[0];
    t ^= t << 11;
    t ^= t >> 8;
    state[0] = state[1];
    state[1] = state[2];
    state[2] = state[3];
    state[3] = state[3] ^ (state[3] >> 19) ^ t;
    state[3]
}

fn next_u32(vm: &mut Vm, receiver: Value) -> u32 {
    let Some(id) = receiver.as_object() else { return 0 };
    let mut state = [0u32; 4];
    match vm.heap.get(id) {
        Some(Object::Instance(instance)) => {
            for (slot, word) in state.iter_mut().enumerate() {
                *word = instance.fields.get(slot).copied().and_then(|v| v.as_num()).unwrap_or(0.0) as u32;
            }
        }
        _ => return 0,
    }
    let value = step(&mut state);
    if let Some(Object::Instance(instance)) = vm.heap.get_mut(id) {
        for (slot, word) in state.iter().enumerate() {
            instance.fields[slot] = Value::num(*word as f64);
        }
    }
    value
}

/// A double in `[0, 1)`, built from 53 bits so every representable value in
/// the range is reachable -- 32 bits would leave most of them unused.
fn next_float(vm: &mut Vm, receiver: Value) -> f64 {
    let high = next_u32(vm, receiver) as u64;
    let low = next_u32(vm, receiver) as u64;
    let bits = ((high << 21) ^ (low >> 11)) & ((1u64 << 53) - 1);
    bits as f64 / (1u64 << 53) as f64
}

#[cfg(feature = "std")]
fn default_seed() -> u32 {
    // Entropy from the clock. Not cryptographic, and not claimed to be -- it
    // exists so two runs of the same program differ.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos() ^ since.as_secs() as u32)
        .unwrap_or(0x1234_5678)
}

#[cfg(not(feature = "std"))]
fn default_seed() -> u32 {
    // **No clock on a bare-metal target**, so an unseeded generator repeats
    // across resets. Saying so is better than inventing entropy that is not
    // there; a firmware that needs a varying stream should seed from something
    // it actually has, such as an ADC reading.
    0x1234_5678
}

/// Build a list value from elements, for the compiler's list literals.
pub fn new_list(vm: &mut Vm, elements: Vec<Value>) -> Value {
    let id = vm.heap.allocate(Object::List(ObjList { elements }));
    Value::object(id)
}
